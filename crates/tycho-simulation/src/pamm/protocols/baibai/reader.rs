//! Reads every configured BaiBai pair over JSON-RPC: the pair data all at one block, from one
//! multicall.
//!
//! A read is one JSON-RPC batch, sent as one HTTP request, at the latest block. It holds one
//! `Multicall3.aggregate3` call: the block's number, timestamp and chain id, the deployment's
//! wiring, the book's settings, custody and, for each pair, its parameters, taker fee and every
//! knot index a side can hold. The curve book reverts for an index past the side's knot count, so
//! only the first `knotCount` answers of each side are kept. Beside it, one `eth_getStorageAt` per
//! proxy reads its EIP-1967 implementation slot, which no contract can read for Multicall3; an
//! implementation other than the pinned one fails the read before any answer is decoded.

use alloy::{
    eips::BlockId,
    primitives::{b256, Address, Bytes as AlloyBytes, B256, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Call3, Result as Call3Result,
        },
        Provider, RootProvider, MULTICALL3_ADDRESS,
    },
    rpc::{
        client::BatchRequest,
        types::{TransactionInput, TransactionRequest},
    },
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

/// EIP-1967's implementation slot, `keccak256("eip1967.proxy.implementation") - 1`.
pub(super) const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

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
    /// Removed from transport errors before they are reported: see [`Self::connection_error`].
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
    /// out; the other pairs are read. Fails fatally when a proxy runs an implementation other than
    /// its pin, when the node serves another chain, or when the deployment no longer answers the
    /// way this integration was built for (its contracts point elsewhere, a getter it depends on
    /// reverts, or a side holds more knots than [`MAX_KNOTS`]), since no later read will either.
    pub(super) async fn read(&self) -> Result<BookRead, FeedError> {
        let mut calls = Vec::new();
        calls.extend(self.settings_calls());
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
    /// custody, in the order [`Self::decode_settings`] destructures their answers. Multicall3's
    /// own views cannot revert; the deployment's may, after an upgrade, and are allowed to so that
    /// a revert is told apart from a node that failed.
    fn settings_calls(&self) -> [Call3; SETTINGS_CALLS] {
        let Deployment { entrypoint, curve_book, custodian, quote, .. } = self.deployment;
        [
            call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode(), false),
            call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode(), false),
            call(MULTICALL3_ADDRESS, getChainIdCall {}.abi_encode(), false),
            call(entrypoint, curveBookCall {}.abi_encode(), true),
            call(entrypoint, custodianCall {}.abi_encode(), true),
            call(entrypoint, quoteTokenCall {}.abi_encode(), true),
            call(curve_book, quoteTokenCall {}.abi_encode(), true),
            call(curve_book, ttlCall {}.abi_encode(), true),
            call(curve_book, cUnitCall {}.abi_encode(), true),
            call(custodian, availableLiquidityCall { token: quote }.abi_encode(), true),
        ]
    }

    /// The read's block and book-wide values, with no pairs yet. The chain id is checked before
    /// any of the deployment's getters is decoded: on another chain they answer from addresses
    /// with no code, successfully and with no data.
    fn decode_settings(&self, answers: &[Call3Result]) -> Result<BookRead, FeedError> {
        let Deployment { chain, curve_book, custodian, quote, .. } = self.deployment;
        let too_few = || FeedError::Parsing("BaiBai multicall answered too few calls".into());
        let [block_number, timestamp, chain_id, wiring @ .., ttl, c_unit, custody] = answers else {
            return Err(too_few());
        };
        let [entry_book, entry_custodian, entry_quote, book_quote] = wiring else {
            return Err(too_few());
        };
        let served = to_u64(setting::<getChainIdCall>(chain_id, "chain id")?)?;
        if served != chain.id() {
            return Err(FeedError::Fatal(format!(
                "BaiBai: the RPC node serves chain {served}, not the deployment's chain {}",
                chain.id()
            )));
        }
        let wiring = [
            (
                "entrypoint's curveBook",
                setting::<curveBookCall>(entry_book, "entrypoint's curveBook")?,
                curve_book,
            ),
            (
                "entrypoint's custodian",
                setting::<custodianCall>(entry_custodian, "entrypoint's custodian")?,
                custodian,
            ),
            (
                "entrypoint's quoteToken",
                setting::<quoteTokenCall>(entry_quote, "entrypoint's quoteToken")?,
                quote,
            ),
            (
                "curve book's quoteToken",
                setting::<quoteTokenCall>(book_quote, "curve book's quoteToken")?,
                quote,
            ),
        ];
        for (what, found, expected) in wiring {
            if found != expected {
                return Err(deployment_changed(format!("the {what} is {found}, not {expected}")));
            }
        }
        Ok(BookRead {
            block_number: to_u64(setting::<getBlockNumberCall>(block_number, "block number")?)?,
            timestamp: to_u64(setting::<getCurrentBlockTimestampCall>(timestamp, "timestamp")?)?,
            ttl: setting::<ttlCall>(ttl, "curve book's ttl")?,
            c_unit: setting::<cUnitCall>(c_unit, "curve book's cUnit")?,
            available_quote: setting::<availableLiquidityCall>(
                custody,
                "custodian's availableLiquidity",
            )?,
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

    /// Sends `calls` in one `aggregate3` at the latest block, batched with a read of each proxy's
    /// implementation slot. Fails fatally when a proxy runs an implementation other than its pin,
    /// before the multicall's answer is looked at: after an upgrade, that answer may well fail in
    /// a way that hides the cause.
    async fn aggregate(&self, calls: Vec<Call3>) -> Result<Vec<Call3Result>, FeedError> {
        let expected = calls.len();
        let request = TransactionRequest::default()
            .to(MULTICALL3_ADDRESS)
            .input(TransactionInput::new(
                aggregate3Call { calls }
                    .abi_encode()
                    .into(),
            ));
        let latest = BlockId::latest();
        let client = self.provider.client();
        let mut batch = BatchRequest::new(&client);
        let multicall = batch
            .add_call::<_, AlloyBytes>("eth_call", &(request, latest))
            .map_err(|e| self.connection_error("multicall", e))?;
        let mut slots = Vec::new();
        for (_, proxy, _) in self.deployment.proxies() {
            let slot = U256::from_be_bytes(IMPLEMENTATION_SLOT.0);
            let read = batch
                .add_call::<_, U256>("eth_getStorageAt", &(proxy, slot, latest))
                .map_err(|e| self.connection_error("implementation slot", e))?;
            slots.push(read);
        }
        batch
            .send()
            .await
            .map_err(|e| self.connection_error("batch", e))?;

        let mut found = Vec::with_capacity(slots.len());
        for slot in slots {
            let word = slot
                .await
                .map_err(|e| self.connection_error("implementation slot", e))?;
            found.push(Address::from_word(B256::from(word)));
        }
        self.check_implementations(&found)?;

        let raw = multicall
            .await
            .map_err(|e| self.connection_error("multicall", e))?;
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

    /// Fails fatally when a proxy runs an implementation other than the one it is pinned to.
    fn check_implementations(&self, found: &[Address]) -> Result<(), FeedError> {
        for ((name, proxy, pinned), found) in self
            .deployment
            .proxies()
            .into_iter()
            .zip(found)
        {
            if *found != pinned {
                return Err(FeedError::Fatal(format!(
                    "BaiBai: the {name} proxy {proxy} runs implementation {found}, not the pinned \
                     {pinned}; the deployment was upgraded and the integration needs verifying \
                     against the new implementation"
                )));
            }
        }
        Ok(())
    }

    /// A transport failure, with the RPC URL removed: it commonly carries a key, and the HTTP
    /// client's errors quote the URL they failed on.
    fn connection_error(&self, what: &str, error: impl std::fmt::Display) -> FeedError {
        let error = error
            .to_string()
            .replace(&self.rpc_url, "<rpc url>");
        FeedError::Connection(format!("BaiBai {what}: {error}"))
    }
}

/// Calls before the pairs' in the multicall: see [`BaibaiReader::settings_calls`].
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
/// have listed. So a revert of `pair` or `validUntil` for any base, or of custody, the taker fee
/// or a knot within the side's count for a listed base, means the deployment changed and fails
/// fatally; so does a side holding more knots than [`MAX_KNOTS`]. A knot past the count reverts
/// by design and is ignored.
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

/// One setting's answer, decoded; fails fatally when its getter reverted, which only the
/// deployment's can.
fn setting<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    if !answer.success {
        return Err(deployment_changed(format!("the {what} getter reverted")));
    }
    decode::<C>(answer, what)
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
