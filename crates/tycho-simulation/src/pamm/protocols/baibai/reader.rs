//! Reads every configured BaiBai pair over JSON-RPC, all at one block.
//!
//! A read is one `Multicall3.aggregate3` call at the latest block: its number, timestamp and chain
//! id, the deployment's wiring, the book's settings, custody and, for each pair, its parameters,
//! taker fee and every knot index a side can hold. The curve book reverts for an index past the
//! side's knot count, so only the first `knotCount` answers of each side are kept.

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes as AlloyBytes, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Call3, Result as Call3Result,
        },
        Provider, RootProvider, MULTICALL3_ADDRESS,
    },
    rpc::types::{TransactionInput, TransactionRequest},
    sol,
    sol_types::{SolCall, SolValue},
};
use reqwest::Url;
use tracing::debug;

use crate::{
    pamm::protocols::baibai::{
        math::{Knot, Side, MAX_KNOTS},
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

/// The entrypoint's fee for a taker and base, in millionths of the gross output: an unnamed view
/// of the unverified implementation, taking `(address taker, address base)` and identified by its
/// storage reads matching `quoteFor`'s fee lookup.
const TAKER_FEE_SELECTOR: [u8; 4] = [0xa5, 0xcb, 0xd1, 0xb7];

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
    /// The entrypoint's fee for the reader's taker, in millionths of the gross output; `u32::MAX`
    /// when the chain reports one that does not fit.
    pub fee_pips: u32,
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
    /// The caller whose fee is read: the entrypoint charges each caller its own.
    taker: Address,
}

impl BaibaiReader {
    pub(super) fn new(
        rpc_url: Url,
        deployment: Deployment,
        bases: Vec<Address>,
        taker: Address,
    ) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            provider: RootProvider::new_http(rpc_url),
            deployment,
            bases,
            taker,
        }
    }

    /// Reads every configured base at the latest block. A base the book has never listed is left
    /// out; the other pairs are read. Fails fatally when the node serves another chain, or when
    /// the deployment no longer answers the way this integration was built for (its contracts
    /// point elsewhere, a getter it reads reverts, or a side holds more knots than
    /// [`MAX_KNOTS`]), since no later read will either.
    pub(super) async fn read(&self) -> Result<BookRead, FeedError> {
        let mut calls = Vec::new();
        for (_, settings_call) in self.settings_calls() {
            calls.push(settings_call);
        }
        for base in &self.bases {
            calls.extend(self.pair_calls(*base));
        }
        let answers = self.aggregate(calls).await?;
        let (settings_answers, pair_answers) = answers.split_at(SETTINGS_CALLS);
        let mut read = self.decode_settings(settings_answers)?;
        for (base, answers) in self
            .bases
            .iter()
            .zip(pair_answers.chunks(PAIR_CALLS))
        {
            if let Some(pair) = decode_pair(*base, answers)? {
                read.pairs.push(pair);
            }
        }
        Ok(read)
    }

    /// The block and chain, the deployment's wiring, the book's settings and the quote token's
    /// custody, each with the getter's name. Multicall3's own views cannot revert; the
    /// deployment's may, after an upgrade, and are allowed to so that [`Self::decode_settings`]
    /// can tell that apart from a node that failed.
    fn settings_calls(&self) -> [(&'static str, Call3); SETTINGS_CALLS] {
        let Deployment { entrypoint, curve_book, custodian, quote, .. } = self.deployment;
        [
            ("block number", call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode(), false)),
            (
                "block timestamp",
                call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode(), false),
            ),
            ("chain id", call(MULTICALL3_ADDRESS, getChainIdCall {}.abi_encode(), false)),
            ("entrypoint's curveBook", call(entrypoint, curveBookCall {}.abi_encode(), true)),
            ("entrypoint's custodian", call(entrypoint, custodianCall {}.abi_encode(), true)),
            ("entrypoint's quoteToken", call(entrypoint, quoteTokenCall {}.abi_encode(), true)),
            ("curve book's quoteToken", call(curve_book, quoteTokenCall {}.abi_encode(), true)),
            ("curve book's ttl", call(curve_book, ttlCall {}.abi_encode(), true)),
            ("curve book's cUnit", call(curve_book, cUnitCall {}.abi_encode(), true)),
            (
                "custodian's availableLiquidity",
                call(custodian, availableLiquidityCall { token: quote }.abi_encode(), true),
            ),
        ]
    }

    /// The read's block and book-wide values, with no pairs yet.
    fn decode_settings(&self, answers: &[Call3Result]) -> Result<BookRead, FeedError> {
        let Deployment { chain, entrypoint, curve_book, custodian, quote } = self.deployment;
        for ((what, _), answer) in self
            .settings_calls()
            .iter()
            .zip(answers)
        {
            if !answer.success {
                return Err(FeedError::Fatal(format!(
                    "BaiBai entrypoint {entrypoint}: the {what} getter reverted; the deployment \
                     changed and the integration needs updating"
                )));
            }
        }
        let served = to_u64(decode::<getChainIdCall>(&answers[2], "chain id")?)?;
        let chain_id = chain.id();
        if served != chain_id {
            return Err(FeedError::Fatal(format!(
                "BaiBai: the RPC node serves chain {served}, not the deployment's chain {chain_id}"
            )));
        }
        let wiring = [
            (
                "entrypoint's curve book",
                decode::<curveBookCall>(&answers[3], "curveBook")?,
                curve_book,
            ),
            (
                "entrypoint's custodian",
                decode::<custodianCall>(&answers[4], "custodian")?,
                custodian,
            ),
            (
                "entrypoint's quote token",
                decode::<quoteTokenCall>(&answers[5], "quoteToken")?,
                quote,
            ),
            (
                "curve book's quote token",
                decode::<quoteTokenCall>(&answers[6], "quoteToken")?,
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
            ttl: decode::<ttlCall>(&answers[7], "ttl")?,
            c_unit: decode::<cUnitCall>(&answers[8], "cUnit")?,
            available_quote: decode::<availableLiquidityCall>(&answers[9], "availableLiquidity")?,
            pairs: Vec::new(),
        })
    }

    /// The pair's parameters, validity, custody and taker fee, then [`MAX_KNOTS`] knot calls for
    /// the ask side and as many for the bid side: [`PAIR_CALLS`] in all.
    fn pair_calls(&self, base: Address) -> Vec<Call3> {
        let Deployment { entrypoint, curve_book, custodian, .. } = self.deployment;
        let mut calls = vec![
            call(curve_book, pairCall { base }.abi_encode(), true),
            call(curve_book, validUntilCall { base }.abi_encode(), true),
            call(custodian, availableLiquidityCall { token: base }.abi_encode(), true),
            call(entrypoint, taker_fee_call(self.taker, base), true),
        ];
        for side in [ASK_SIDE, BID_SIDE] {
            for index in 0..MAX_KNOTS {
                let knot = knotCall { base, side, index: U256::from(index) };
                calls.push(call(curve_book, knot.abi_encode(), true));
            }
        }
        calls
    }

    async fn aggregate(&self, calls: Vec<Call3>) -> Result<Vec<Call3Result>, FeedError> {
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
            .block(BlockId::latest())
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

/// Calls of the first multicall before the pairs': see [`BaibaiReader::settings_calls`].
const SETTINGS_CALLS: usize = 10;

/// Calls per configured base before its knots: see [`BaibaiReader::pair_calls`].
const PAIR_HEAD_CALLS: usize = 4;

/// Calls per configured base: see [`BaibaiReader::pair_calls`].
const PAIR_CALLS: usize = PAIR_HEAD_CALLS + 2 * MAX_KNOTS;

/// The entrypoint's fee for `taker` on `base`, as calldata.
pub(super) fn taker_fee_call(taker: Address, base: Address) -> Vec<u8> {
    let mut data = TAKER_FEE_SELECTOR.to_vec();
    data.extend((taker, base).abi_encode_params());
    data
}

fn call(target: Address, data: Vec<u8>, allow_failure: bool) -> Call3 {
    Call3 { target, allowFailure: allow_failure, callData: AlloyBytes::from(data) }
}

/// One configured base's pair with its knots; `None` when the book has not listed it.
///
/// The curve book answers `pair` and `validUntil` for any address, with zeros for one it has not
/// listed, and the custodian reverts only for an address that is no token, which the book cannot
/// have listed. So a revert of any call for a listed base, or of `pair` or `validUntil` for any
/// base, means the deployment changed, and fails fatally; so does a side holding more knots than
/// [`MAX_KNOTS`].
fn decode_pair(base: Address, answers: &[Call3Result]) -> Result<Option<PairRead>, FeedError> {
    let (head, knots) = answers.split_at(PAIR_HEAD_CALLS);
    let [pair, valid_until, available, fee] = head else {
        return Err(FeedError::Parsing("BaiBai multicall answered too few calls".into()));
    };
    let (ask_knots, bid_knots) = knots.split_at(MAX_KNOTS);
    for (getter, answer) in [("curve book's pair", pair), ("curve book's validUntil", valid_until)]
    {
        if !answer.success {
            return Err(deployment_changed(format!("the {getter} getter reverted for {base}")));
        }
    }
    let view = decode::<pairCall>(pair, "pair")?;
    if view.qUnit == U256::ZERO {
        debug!(%base, "skipping base the BaiBai curve book has not listed");
        return Ok(None);
    }
    for (getter, answer) in
        [("custodian's availableLiquidity", available), ("entrypoint's taker fee", fee)]
    {
        if !answer.success {
            return Err(deployment_changed(format!(
                "the {getter} getter reverted for listed base {base}"
            )));
        }
    }
    for (name, side) in [("ask", &view.ask), ("bid", &view.bid)] {
        if usize::from(side.knotCount) > MAX_KNOTS {
            return Err(deployment_changed(format!(
                "{base}'s {name} side holds {} knots, more than the {MAX_KNOTS} read",
                side.knotCount
            )));
        }
    }
    let fee = U256::abi_decode(&fee.returnData)
        .map_err(|e| FeedError::Parsing(format!("BaiBai taker fee: {e}")))?;
    let ask = read_knots(base, "ask", ask_knots, view.ask.knotCount)?;
    let bid = read_knots(base, "bid", bid_knots, view.bid.knotCount)?;
    let side = |side: &SideView, knots: Vec<Knot>| Side {
        spread_bps: side.spreadBps,
        depth_bps: side.depthBps,
        filled: side.filled,
        knots,
    };
    let pair = PairRead {
        base,
        last_update_at: view.lastUpdateAt,
        valid_until: decode::<validUntilCall>(valid_until, "validUntil")?,
        mid: U256::from(view.mid),
        q_unit: view.qUnit,
        available_base: decode::<availableLiquidityCall>(available, "availableLiquidity")?,
        fee_pips: fee.try_into().unwrap_or(u32::MAX),
        ask: side(&view.ask, ask),
        bid: side(&view.bid, bid),
    };
    Ok(Some(pair))
}

/// The first `count` of one side's knot answers. The answers past `count` revert at the curve
/// book and are ignored; one within it reverting means the deployment changed.
fn read_knots(
    base: Address,
    side: &str,
    answers: &[Call3Result],
    count: u8,
) -> Result<Vec<Knot>, FeedError> {
    let mut knots = Vec::with_capacity(usize::from(count));
    for (index, answer) in answers[..usize::from(count)]
        .iter()
        .enumerate()
    {
        if !answer.success {
            return Err(deployment_changed(format!(
                "the curve book's knot getter reverted for {base}'s {side} knot {index} of {count}"
            )));
        }
        let knot = decode::<knotCall>(answer, "knot")?;
        knots.push(Knot { q: knot.q.to(), extra: knot.extra.to() });
    }
    Ok(knots)
}

/// The fatal error for a deployment that no longer answers the way this integration was built for.
fn deployment_changed(what: String) -> FeedError {
    FeedError::Fatal(format!(
        "BaiBai: {what}; the deployment changed and the integration needs updating"
    ))
}

fn to_u64(value: U256) -> Result<u64, FeedError> {
    value
        .try_into()
        .map_err(|e| FeedError::Parsing(format!("BaiBai block value {value}: {e}")))
}

fn decode<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    C::abi_decode_returns(&answer.returnData)
        .map_err(|e| FeedError::Parsing(format!("BaiBai {what}: {e}")))
}
