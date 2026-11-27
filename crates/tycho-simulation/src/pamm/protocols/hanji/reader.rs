//! Reads every listed Hanji market over JSON-RPC, all at one block, in one multicall.
//!
//! A proxy orderbook trades against virtual orders that one of two quoters places for the caller:
//! its FastQuoter for the transaction's sender and for callers on its whitelist, and a second
//! quoter, a few bps wider, for any other contract, such as a router (see the module docs). Each
//! quoter reports the book it would place with `assembleOrderbookFromOrders(lob, isAsk, depth)`,
//! where `lob` is the market's underlying CLOB.
//!
//! Hanji's documentation points aggregators to `FastQuoterHelper.assembleOrderbookFromOrders`,
//! which forwards to FastQuoter only, so it reports the book of the transaction's sender and
//! whitelisted callers and never the wider one other contracts are filled against. The reader
//! asks both quoters directly instead.
//!
//! A read is one `eth_call` of `Multicall3.aggregate3` at the latest block: the block's number,
//! timestamp and chain id, then for each market its proxy's `getConfig()`, whether it is paused,
//! its wiring (CLOB and quoters), whether the taker is whitelisted, and both quoters' books. The
//! wiring is resolved once per market, by a multicall of its own before the first read that lists
//! the market, and checked on every read after; a market whose wiring changed is left out of that
//! read and resolved again before the next. A market whose wiring does not resolve, or that names
//! no contract quoter, is not read, and its wiring is asked for again [`UNSERVED_RETRY`] later.
//!
//! After the multicall, the reader reads the EIP-1967 implementation slot of each market's proxy
//! and CLOB at the read's block, with one `eth_getStorageAt` each, since no call can read another
//! contract's storage. A market whose proxy or CLOB runs another implementation than
//! [`Deployment::implementations`] pins, or is not pinned, is left out of the read, logged once
//! when that starts and once when it ends. The quoters are not proxies: a proxy that names
//! another quoter changes its wiring, which every read checks.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use alloy::{
    eips::BlockId,
    primitives::{aliases::U24, b256, Address, Bytes as AlloyBytes, B256, U256},
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
use futures::future::try_join_all;
use reqwest::Url;
use tracing::{debug, error, info, warn};

use crate::{
    pamm::protocols::hanji::{
        market::{Market, NativeSide},
        state::Level,
        Deployment,
    },
    snapshot_feed::errors::FeedError,
};

// The proxy orderbook's config, as Hanji documents it at
// <https://docs.hanji.io/for-developers/liquidity-source-integration>, its whitelist, and the
// quoters' book view, which FastQuoterHelper forwards to with the same arguments.
sol! {
    #[derive(Debug)]
    function getConfig() external view returns (
        uint256 scalingFactorTokenX,
        uint256 scalingFactorTokenY,
        address tokenX,
        address tokenY,
        bool supportsNativeEth,
        bool isTokenXWeth,
        address askTrie,
        address bidTrie,
        uint64 adminCommissionRate,
        uint64 totalAggressiveCommissionRate,
        uint64 totalPassiveCommissionRate,
        uint64 passiveOrderPayoutRate,
        bool shouldInvokeOnTrade
    );

    #[derive(Debug)]
    function whitelisted(address caller) external view returns (bool);

    #[derive(Debug)]
    function paused() external view returns (bool);

    #[derive(Debug)]
    function assembleOrderbookFromOrders(address lobAddress, bool isAsk, uint24 maxPriceLevels)
        external view returns (uint72[] memory arrayPrices, uint128[] memory arrayShares);

}

/// Unnamed getters of the proxy orderbooks' unverified implementations, each identified by what
/// it returns on the Base markets: the underlying CLOB, the FastQuoter that FastQuoterHelper
/// forwards to, and the quoter the proxy calls for a contract that is neither the transaction's
/// sender nor whitelisted (zero on the tokenized-equity markets, which name none).
pub(super) const LOB_SELECTOR: [u8; 4] = [0xf7, 0xd7, 0x45, 0x24];
pub(super) const QUOTER_SELECTOR: [u8; 4] = [0x93, 0x8c, 0x51, 0x32];
pub(super) const CONTRACT_QUOTER_SELECTOR: [u8; 4] = [0xf8, 0xbe, 0xd2, 0x5f];

/// How many price levels a read asks for per side. FastQuoter's books held five per side when
/// this integration was written; a side holding more is read up to this many, so its depth past
/// them is not offered.
pub(super) const MAX_PRICE_LEVELS: U24 = U24::from_limbs([32]);

/// Calls before the markets' in the read: block number, timestamp and chain id.
const HEAD_CALLS: usize = 3;

/// Calls per market in the read: config, pause, the three wiring getters, the taker's whitelist
/// entry, then asks and bids from the quoter and from the contract quoter.
const MARKET_CALLS: usize = 10;

/// Calls per market in a wiring resolution: see [`wiring_calls`].
const WIRING_CALLS: usize = 3;

/// How long a market that is not read, because its wiring did not resolve or it names no
/// contract quoter, waits before its wiring is asked for again.
const UNSERVED_RETRY: Duration = Duration::from_secs(30);

/// EIP-1967's implementation slot, `keccak256("eip1967.proxy.implementation") - 1`.
const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Where a market's virtual orders come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Wiring {
    pub lob: Address,
    pub quoter: Address,
    pub contract_quoter: Address,
}

/// A market's wiring as last resolved.
#[derive(Debug, Clone, Copy)]
enum Resolution {
    Wired(Wiring),
    /// Not read until `retry_at`: see [`UNSERVED_RETRY`].
    Unserved {
        retry_at: Instant,
    },
}

/// A market's config as the reader uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MarketConfig {
    pub scaling_x: U256,
    pub scaling_y: U256,
    /// `totalAggressiveCommissionRate + passiveOrderPayoutRate`, per 1e18 of the executed value.
    pub fee_rate: u64,
}

/// One market as the chain reported it, checked against the market list, with the book its
/// proxy quotes the taker.
#[derive(Debug, Clone)]
pub(super) struct MarketRead {
    pub market: Market,
    pub config: MarketConfig,
    /// Best first, as the quoters sort them.
    pub asks: Vec<Level>,
    pub bids: Vec<Level>,
}

/// Everything one read returned, at one block.
#[derive(Debug, Clone)]
pub(super) struct BookRead {
    pub block_number: u64,
    pub timestamp: u64,
    /// The markets that read cleanly, in the order they were asked for.
    pub markets: Vec<MarketRead>,
}

#[derive(Clone, derive_more::Debug)]
pub(super) struct HanjiReader {
    #[debug(skip)]
    provider: RootProvider,
    /// Removed from transport errors before they are reported: see [`Self::connection_error`].
    #[debug(skip)]
    rpc_url: String,
    deployment: Deployment,
    /// The caller whose book is read: Tycho's router unless the feed was built for another.
    taker: Address,
    /// Each proxy's wiring, as last resolved.
    #[debug(skip)]
    wiring: Arc<Mutex<HashMap<Address, Resolution>>>,
    /// The proxies whose market the last read left out for its implementations, so that the
    /// change is logged once each way.
    #[debug(skip)]
    withheld: Arc<Mutex<HashSet<Address>>>,
}

impl HanjiReader {
    pub(super) fn new(rpc_url: Url, deployment: Deployment, taker: Address) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            provider: RootProvider::new_http(rpc_url),
            deployment,
            taker,
            wiring: Arc::new(Mutex::new(HashMap::new())),
            withheld: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Reads `markets` at the latest block, those whose wiring [`Self::resolve_wiring`] resolved.
    /// A market whose calls revert, whose config names other tokens or another native side than
    /// the list, or whose wiring changed is left out, and the read logs how many with the first
    /// reason; it fails when every market was left out that way or none is served (see the module
    /// docs), and fails fatally when the node serves another chain. A paused market, and one whose
    /// proxy or CLOB runs an implementation not pinned, is left out without failing anything; a
    /// node that cannot report the implementations fails the read.
    pub(super) async fn read(&self, markets: &[Market]) -> Result<BookRead, FeedError> {
        let wired = self.wired(markets);
        if wired.is_empty() {
            return Err(FeedError::Parsing(format!(
                "Hanji: none of the {} listed markets is served: each one's wiring failed to \
                 resolve or names no contract quoter",
                markets.len()
            )));
        }
        let mut calls = vec![
            call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode()),
            call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode()),
            call(MULTICALL3_ADDRESS, getChainIdCall {}.abi_encode()),
        ];
        for (market, wiring) in &wired {
            calls.extend(self.market_calls(market.orderbook, wiring));
        }
        let answers = self.aggregate(calls).await?;
        let (head, market_answers) = answers.split_at(HEAD_CALLS);
        let mut read = self.decode_head(head)?;
        let mut failures: Vec<(Address, String)> = Vec::new();
        let mut decoded: Vec<(MarketRead, Address)> = Vec::new();
        // `aggregate` checked the answer count, so the chunks are whole and one per market.
        let (market_answers, _) = market_answers.as_chunks::<MARKET_CALLS>();
        for ((market, wiring), answers) in wired.iter().zip(market_answers) {
            match self.decode_market(*market, wiring, answers) {
                Ok(Some(market_read)) => decoded.push((market_read, wiring.lob)),
                Ok(None) => {}
                Err(reason) => failures.push((market.orderbook, reason)),
            }
        }
        let decoded_any = !decoded.is_empty();
        read.markets = self
            .pinned(read.block_number, decoded)
            .await?;
        let Some((orderbook, reason)) = failures.first() else {
            return Ok(read);
        };
        if !decoded_any {
            return Err(FeedError::Parsing(format!(
                "Hanji: none of the {} markets read cleanly; {orderbook} failed with: {reason}",
                wired.len()
            )));
        }
        warn!(skipped = failures.len(), %orderbook, %reason, "skipping Hanji markets");
        Ok(read)
    }

    /// The markets of `decoded` (each with its CLOB) whose proxy and CLOB run the implementations
    /// the deployment pins, read at `block`.
    async fn pinned(
        &self,
        block: u64,
        decoded: Vec<(MarketRead, Address)>,
    ) -> Result<Vec<MarketRead>, FeedError> {
        let contracts: Vec<Address> = decoded
            .iter()
            .flat_map(|(market_read, lob)| [market_read.market.orderbook, *lob])
            .collect();
        let implementations: HashMap<Address, Address> = try_join_all(
            contracts
                .iter()
                .map(|contract| self.implementation(*contract, block)),
        )
        .await?
        .into_iter()
        .collect();
        let mut withheld = self
            .withheld
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut pinned = Vec::new();
        for (market_read, lob) in decoded {
            let orderbook = market_read.market.orderbook;
            let refusal = [orderbook, lob]
                .into_iter()
                .find_map(|contract| {
                    let found = implementations
                        .get(&contract)
                        .copied()?;
                    let reason = unpinned(
                        self.deployment
                            .pinned_implementation(contract),
                        found,
                    )?;
                    Some(format!("{contract} {reason}"))
                });
            match refusal {
                Some(reason) => {
                    if withheld.insert(orderbook) {
                        error!(%orderbook, block, %reason, "withholding Hanji market: its \
                            implementation is not the one its fills were recorded against");
                    }
                }
                None => {
                    if withheld.remove(&orderbook) {
                        info!(%orderbook, block, "Hanji market runs its pinned implementations \
                            again");
                    }
                    pinned.push(market_read);
                }
            }
        }
        Ok(pinned)
    }

    /// The implementation in `proxy`'s EIP-1967 slot at `block`.
    async fn implementation(
        &self,
        proxy: Address,
        block: u64,
    ) -> Result<(Address, Address), FeedError> {
        let word = self
            .provider
            .get_storage_at(proxy, IMPLEMENTATION_SLOT.into())
            .block_id(BlockId::number(block))
            .await
            .map_err(|e| self.connection_error("implementation slot", e))?;
        Ok((proxy, Address::from_word(B256::from(word))))
    }

    /// The markets whose wiring is resolved and served, with it.
    fn wired(&self, markets: &[Market]) -> Vec<(Market, Wiring)> {
        let cache = self
            .wiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        markets
            .iter()
            .filter_map(|market| match cache.get(&market.orderbook)? {
                Resolution::Wired(wiring) => Some((*market, *wiring)),
                Resolution::Unserved { retry_at: _ } => None,
            })
            .collect()
    }

    /// Resolves, in one multicall, the wiring of every market not yet resolved or whose
    /// [`UNSERVED_RETRY`] has passed. A market whose getters revert, or that names no contract
    /// quoter, is left out of the reads until then.
    pub(super) async fn resolve_wiring(&self, markets: &[Market]) -> Result<(), FeedError> {
        let now = Instant::now();
        let unresolved: Vec<Address> = {
            let cache = self
                .wiring
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            markets
                .iter()
                .map(|market| market.orderbook)
                .filter(|orderbook| match cache.get(orderbook) {
                    None => true,
                    Some(Resolution::Wired(_)) => false,
                    Some(Resolution::Unserved { retry_at }) => now >= *retry_at,
                })
                .collect()
        };
        if unresolved.is_empty() {
            return Ok(());
        }
        // The chain id goes first: on another chain the proxies have no code, so their getters
        // answer nothing and every market would look unserved rather than misconfigured.
        let mut calls = vec![call(MULTICALL3_ADDRESS, getChainIdCall {}.abi_encode())];
        calls.extend(
            unresolved
                .iter()
                .flat_map(|orderbook| wiring_calls(*orderbook)),
        );
        let answers = self.aggregate(calls).await?;
        let (chain_id, answers) = answers.split_at(1);
        self.check_chain(&chain_id[0])?;
        // `aggregate` checked the answer count, so the chunks are whole and one per market.
        let (answers, _) = answers.as_chunks::<WIRING_CALLS>();
        let unserved = Resolution::Unserved { retry_at: now + UNSERVED_RETRY };
        let mut cache = self
            .wiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (orderbook, [lob, quoter, contract_quoter]) in unresolved.iter().zip(answers) {
            let resolution = match decode_wiring(lob, quoter, contract_quoter) {
                Ok(Some(wiring)) => Resolution::Wired(wiring),
                Ok(None) => {
                    debug!(%orderbook, retry_in = ?UNSERVED_RETRY, "not reading Hanji market \
                        with no contract quoter");
                    unserved
                }
                Err(reason) => {
                    warn!(%orderbook, %reason, retry_in = ?UNSERVED_RETRY, "cannot resolve \
                        Hanji market's wiring");
                    unserved
                }
            };
            cache.insert(*orderbook, resolution);
        }
        Ok(())
    }

    /// The market's calls in the order [`Self::decode_market`] reads them. Each may revert, so
    /// that one market's revert leaves the others' answers standing.
    fn market_calls(&self, orderbook: Address, wiring: &Wiring) -> [Call3; MARKET_CALLS] {
        let [lob, quoter, contract] = wiring_calls(orderbook);
        [
            call(orderbook, getConfigCall {}.abi_encode()),
            call(orderbook, pausedCall {}.abi_encode()),
            lob,
            quoter,
            contract,
            call(orderbook, whitelistedCall { caller: self.taker }.abi_encode()),
            call(wiring.quoter, book_call(wiring.lob, true)),
            call(wiring.quoter, book_call(wiring.lob, false)),
            call(wiring.contract_quoter, book_call(wiring.lob, true)),
            call(wiring.contract_quoter, book_call(wiring.lob, false)),
        ]
    }

    /// The read's block, with no markets yet. The chain id is checked before anything else is
    /// decoded: on another chain the markets answer from addresses with no code.
    fn decode_head(&self, head: &[Call3Result]) -> Result<BookRead, FeedError> {
        let [block_number, timestamp, chain_id] = head else {
            return Err(FeedError::Parsing("Hanji multicall answered too few calls".into()));
        };
        self.check_chain(chain_id)?;
        let block_number = decode::<getBlockNumberCall>(block_number, "block number")?;
        let timestamp = decode::<getCurrentBlockTimestampCall>(timestamp, "timestamp")?;
        Ok(BookRead {
            block_number: to_u64(block_number, "block number")?,
            timestamp: to_u64(timestamp, "timestamp")?,
            markets: Vec::new(),
        })
    }

    /// Fails fatally when the node serves another chain than the deployment's.
    fn check_chain(&self, answer: &Call3Result) -> Result<(), FeedError> {
        let served = to_u64(decode::<getChainIdCall>(answer, "chain id")?, "chain id")?;
        let chain = self.deployment.chain;
        if served != chain.id() {
            return Err(FeedError::Fatal(format!(
                "Hanji: the RPC node serves chain {served}, not the deployment's chain {}",
                chain.id()
            )));
        }
        Ok(())
    }

    /// One market's answers: `Ok(None)`, logged, when the proxy is paused, `Err` when it cannot
    /// be served from them. A changed wiring is forgotten, to be resolved again.
    fn decode_market(
        &self,
        market: Market,
        wiring: &Wiring,
        answers: &[Call3Result; MARKET_CALLS],
    ) -> Result<Option<MarketRead>, String> {
        let [config, paused, lob, quoter, contract, whitelisted, quoter_asks, quoter_bids, contract_asks, contract_bids] =
            answers;
        let found = decode_wiring(lob, quoter, contract);
        if found != Ok(Some(*wiring)) {
            self.wiring
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&market.orderbook);
            return Err(match found {
                Ok(found) => format!("its wiring changed from {wiring:?} to {found:?}"),
                Err(reason) => format!("its wiring no longer resolves: {reason}"),
            });
        }
        let config = decode_config(market, config)?;
        if decode_answer::<pausedCall>(paused, "paused")? {
            debug!(orderbook = %market.orderbook, "skipping paused Hanji market");
            return Ok(None);
        }
        let whitelisted = decode_answer::<whitelistedCall>(whitelisted, "whitelisted")?;
        let (asks, bids) =
            if whitelisted { (quoter_asks, quoter_bids) } else { (contract_asks, contract_bids) };
        Ok(Some(MarketRead {
            market,
            config,
            asks: decode_levels(asks, "asks")?,
            bids: decode_levels(bids, "bids")?,
        }))
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
            .map_err(|e| self.connection_error("multicall", e))?;
        let answers = aggregate3Call::abi_decode_returns(&raw)
            .map_err(|e| FeedError::Parsing(format!("Hanji multicall: {e}")))?;
        if answers.len() != expected {
            return Err(FeedError::Parsing(format!(
                "Hanji multicall answered {} of {expected} calls",
                answers.len()
            )));
        }
        Ok(answers)
    }

    /// A transport failure, with the RPC URL removed: it commonly carries a key, and the HTTP
    /// client's errors quote the URL they failed on.
    fn connection_error(&self, what: &str, error: impl std::fmt::Display) -> FeedError {
        let error = error
            .to_string()
            .replace(&self.rpc_url, "<rpc url>");
        FeedError::Connection(format!("Hanji {what}: {error}"))
    }
}

/// Why a contract running `found` is not served, given the implementation pinned for it: `None`
/// when it runs the pinned one.
fn unpinned(pinned: Option<Address>, found: Address) -> Option<String> {
    match pinned {
        None => Some(format!("runs implementation {found}, and none is pinned for it")),
        Some(pinned) if pinned != found => {
            Some(format!("runs implementation {found}, not the pinned {pinned}"))
        }
        Some(_) => None,
    }
}

/// The market's config, checked against the list: the same tokens with the native token on
/// the same side (which `Market::from_api` has checked is the wrapped native).
fn decode_config(market: Market, answer: &Call3Result) -> Result<MarketConfig, String> {
    let config = decode_answer::<getConfigCall>(answer, "getConfig")?;
    if (config.tokenX, config.tokenY) != (market.token_x, market.token_y) {
        return Err(format!(
            "getConfig trades {} against {}, the market list {} against {}",
            config.tokenX, config.tokenY, market.token_x, market.token_y
        ));
    }
    let native = match (config.supportsNativeEth, config.isTokenXWeth) {
        (false, _) => NativeSide::None,
        (true, true) => NativeSide::X,
        (true, false) => NativeSide::Y,
    };
    if native != market.native {
        return Err(format!(
            "getConfig makes {native:?} the native side, the market list {:?}",
            market.native
        ));
    }
    let fee_rate = config
        .totalAggressiveCommissionRate
        .checked_add(config.passiveOrderPayoutRate)
        .ok_or("its commission rates overflow")?;
    Ok(MarketConfig {
        scaling_x: config.scalingFactorTokenX,
        scaling_y: config.scalingFactorTokenY,
        fee_rate,
    })
}

/// The proxy's CLOB, quoter and contract quoter getters, in the order [`decode_wiring`] reads
/// them.
fn wiring_calls(orderbook: Address) -> [Call3; WIRING_CALLS] {
    [
        call(orderbook, LOB_SELECTOR.to_vec()),
        call(orderbook, QUOTER_SELECTOR.to_vec()),
        call(orderbook, CONTRACT_QUOTER_SELECTOR.to_vec()),
    ]
}

/// The wiring the getters answered, `None` where the proxy names no contract quoter.
fn decode_wiring(
    lob: &Call3Result,
    quoter: &Call3Result,
    contract_quoter: &Call3Result,
) -> Result<Option<Wiring>, String> {
    let address = |answer: &Call3Result, what: &str| -> Result<Address, String> {
        if !answer.success {
            return Err(format!("its {what} getter reverted"));
        }
        Address::abi_decode(&answer.returnData)
            .map_err(|e| format!("its {what} did not decode: {e}"))
    };
    let lob = address(lob, "CLOB")?;
    if lob.is_zero() {
        return Err("it names no CLOB".into());
    }
    let quoter = address(quoter, "quoter")?;
    if quoter.is_zero() {
        return Err("it names no quoter".into());
    }
    let contract_quoter = address(contract_quoter, "contract quoter")?;
    Ok((!contract_quoter.is_zero()).then_some(Wiring { lob, quoter, contract_quoter }))
}

fn book_call(lob: Address, is_ask: bool) -> Vec<u8> {
    assembleOrderbookFromOrdersCall {
        lobAddress: lob,
        isAsk: is_ask,
        maxPriceLevels: MAX_PRICE_LEVELS,
    }
    .abi_encode()
}

/// Every call may fail, so that a reverting market is told apart from a node that failed.
fn call(target: Address, data: Vec<u8>) -> Call3 {
    Call3 { target, allowFailure: true, callData: AlloyBytes::from(data) }
}

fn decode_levels(answer: &Call3Result, side: &str) -> Result<Vec<Level>, String> {
    let levels = decode_answer::<assembleOrderbookFromOrdersCall>(answer, side)?;
    if levels.arrayPrices.len() != levels.arrayShares.len() {
        return Err(format!(
            "its {side} hold {} prices for {} share counts",
            levels.arrayPrices.len(),
            levels.arrayShares.len()
        ));
    }
    Ok(levels
        .arrayPrices
        .iter()
        .zip(&levels.arrayShares)
        .map(|(price, shares)| Level { price: U256::from(*price), shares: U256::from(*shares) })
        .collect())
}

fn decode_answer<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, String> {
    if !answer.success {
        return Err(format!("{what} reverted"));
    }
    C::abi_decode_returns(&answer.returnData).map_err(|e| format!("{what} did not decode: {e}"))
}

/// A head call's answer; Multicall3's own views do not revert.
fn decode<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    decode_answer::<C>(answer, what).map_err(|e| FeedError::Parsing(format!("Hanji {e}")))
}

fn to_u64(value: U256, what: &str) -> Result<u64, FeedError> {
    value
        .try_into()
        .map_err(|e| FeedError::Parsing(format!("Hanji {what} {value}: {e}")))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{aliases::U72, hex, keccak256};
    use rstest::rstest;

    use super::*;

    fn answer(success: bool, data: Vec<u8>) -> Call3Result {
        Call3Result { success, returnData: data.into() }
    }

    fn address_answer(byte: u8) -> Call3Result {
        answer(true, Address::repeat_byte(byte).abi_encode())
    }

    #[test]
    fn wiring_names_the_clob_and_both_quoters() {
        let wiring = decode_wiring(&address_answer(1), &address_answer(2), &address_answer(3));

        assert_eq!(
            wiring,
            Ok(Some(Wiring {
                lob: Address::repeat_byte(1),
                quoter: Address::repeat_byte(2),
                contract_quoter: Address::repeat_byte(3),
            }))
        );
    }

    #[test]
    fn wiring_without_a_contract_quoter_is_not_served() {
        let wiring = decode_wiring(&address_answer(1), &address_answer(2), &address_answer(0));

        assert_eq!(wiring, Ok(None));
    }

    #[rstest]
    #[case::clob_reverts(answer(false, Vec::new()), address_answer(2), "CLOB getter reverted")]
    #[case::no_clob(address_answer(0), address_answer(2), "names no CLOB")]
    #[case::quoter_reverts(address_answer(1), answer(false, Vec::new()), "quoter getter reverted")]
    #[case::no_quoter(address_answer(1), address_answer(0), "names no quoter")]
    #[case::no_code(address_answer(1), answer(true, Vec::new()), "quoter did not decode")]
    fn wiring_that_names_no_book_is_refused(
        #[case] lob: Call3Result,
        #[case] quoter: Call3Result,
        #[case] reason: &str,
    ) {
        let wiring = decode_wiring(&lob, &quoter, &address_answer(3));

        let error = wiring.unwrap_err();
        assert!(error.contains(reason), "{error}");
    }

    #[test]
    fn only_the_pinned_implementation_is_served() {
        let (pinned, other) = (Address::repeat_byte(1), Address::repeat_byte(2));

        let changed = unpinned(Some(pinned), other).unwrap();
        let not_pinned = unpinned(None, other).unwrap();

        assert_eq!(unpinned(Some(pinned), pinned), None);
        assert!(changed.contains("not the pinned"), "{changed}");
        assert!(not_pinned.contains("none is pinned"), "{not_pinned}");
    }

    #[test]
    fn the_implementation_slot_is_eip_1967s() {
        let slot = U256::from_be_bytes(keccak256("eip1967.proxy.implementation").0) - U256::from(1);

        assert_eq!(B256::from(slot), IMPLEMENTATION_SLOT);
    }

    #[test]
    fn a_side_with_more_prices_than_share_counts_is_refused() {
        let side = assembleOrderbookFromOrdersCall::abi_encode_returns(
            &assembleOrderbookFromOrdersReturn {
                arrayPrices: vec![U72::from(2), U72::from(1)],
                arrayShares: vec![5],
            },
        );

        let error = decode_levels(&answer(true, side), "bids").unwrap_err();

        assert!(error.contains("2 prices for 1 share counts"), "{error}");
    }

    /// `testdata/record_fixture.py` records what the feed reads: the same depth, getters, router
    /// and market list.
    #[test]
    fn testdata_constants_match() {
        let script = include_str!("testdata/record_fixture.py");
        let expected = [
            format!("DEPTH = {}", MAX_PRICE_LEVELS),
            format!("LOB_SELECTOR = \"{}\"", hex::encode(LOB_SELECTOR)),
            format!("QUOTER_SELECTOR = \"{}\"", hex::encode(QUOTER_SELECTOR)),
            format!("CONTRACT_QUOTER_SELECTOR = \"{}\"", hex::encode(CONTRACT_QUOTER_SELECTOR)),
            format!("TYCHO_ROUTER = \"{:#x}\"", crate::pamm::protocols::hanji::BASE_ROUTER),
            format!("API = \"{}\"", crate::pamm::protocols::hanji::BASE_DEPLOYMENT.markets_url),
        ];
        for line in expected {
            assert!(script.contains(&line), "record_fixture.py lacks `{line}`");
        }
    }
}
