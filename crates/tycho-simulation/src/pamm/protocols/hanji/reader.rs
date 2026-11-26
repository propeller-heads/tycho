//! Reads every listed Hanji market over JSON-RPC, all at one block, in one multicall.
//!
//! A proxy orderbook trades against virtual orders that one of two quoters places for the caller:
//! its FastQuoter for the transaction's sender and for callers on its whitelist, and a second
//! quoter, a few bps wider, for any other contract, such as a router (see the module docs). Each
//! quoter reports the book it would place with `assembleOrderbookFromOrders(lob, isAsk, depth)`,
//! where `lob` is the market's underlying CLOB.
//!
//! A read is one `eth_call` of `Multicall3.aggregate3` at the latest block: the block's number,
//! timestamp and chain id, then for each market its proxy's `getConfig()`, whether it is paused,
//! its wiring (CLOB and quoters), whether the taker is whitelisted, and both quoters' books. The
//! wiring is resolved once per market, by a multicall of its own before the first read that lists
//! the market, and checked on every read after; a market whose wiring changed is left out of that
//! read and resolved again before the next. A market whose wiring does not resolve, or that names
//! no contract quoter, is not read, and its wiring is asked for again [`UNSERVED_RETRY`] later.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use alloy::{
    eips::BlockId,
    primitives::{
        address,
        aliases::{U24, U72},
        hex, keccak256, Address, Bytes as AlloyBytes, B256, U256,
    },
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Call3, Result as Call3Result,
        },
        Provider, RootProvider, MULTICALL3_ADDRESS,
    },
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionInput, TransactionRequest,
    },
    sol,
    sol_types::{SolCall, SolValue},
    transports::RpcError,
};
use reqwest::Url;
use tracing::{debug, warn};

use crate::{
    pamm::protocols::hanji::{
        market::{Market, NativeSide},
        state::{FillReport, Level, Probe},
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

    #[derive(Debug)]
    function placeOrder(
        bool isAsk,
        uint128 quantity,
        uint72 price,
        uint128 maxCommission,
        bool marketOnly,
        bool postOnly,
        bool transferExecutedTokens,
        uint256 expires
    ) external payable returns (
        uint64 orderId,
        uint128 executedShares,
        uint128 executedValue,
        uint128 aggressiveFee
    );

    #[derive(Debug)]
    function placeMarketOrderWithTargetValue(
        bool isAsk,
        uint128 targetTokenYValue,
        uint72 price,
        uint128 maxCommission,
        bool transferExecutedTokens,
        uint256 expires
    ) external payable returns (uint128 executedShares, uint128 executedValue, uint128 aggressiveFee);
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

/// Runtime code planted at the taker for a fill check: it calls the address in the first 20 bytes
/// of its calldata with the rest and its value, and returns or reverts with the answer. A call
/// without calldata stops, so it takes the native ETH a proxy pays out, as the router does.
pub(super) const FORWARDER: [u8; 51] = hex!(
    "3615603157" // if calldatasize == 0 jump to stop
    "6014360380601460003760006000826000" // calldatacopy(0, 20, size - 20); call's sizes, offsets
    "3460003560601c5af1" // value, target = calldata[0..20], gas, call
    "3d600060003e" // returndatacopy(0, 0, returndatasize)
    "602c573d6000fd" // if the call failed revert(0, returndatasize)
    "5b3d6000f3" // return(0, returndatasize)
    "5b00" // stop
);

/// The sender of a fill check's call. It is not the taker, so the proxy sees the taker as a
/// contract that is not the transaction's sender, as it sees Tycho's router inside a route.
pub(super) const PROBE_SENDER: Address = address!("00000000000000000000000000000000000c4ec4");

/// The prices a fill check accepts, as tycho-execution's `HanjiExecutor` sends them: any bid for a
/// sell, and the highest price the orderbooks accept for a buy.
const MIN_PRICE: U72 = U72::from_limbs([1, 0]);
/// 999,999e15, as limbs.
const MAX_PRICE: U72 = U72::from_limbs([0x35c6_2047_39d9_8000, 0x36]);

/// Balance and allowance mapping slots of the ERC20s a fill check funds, found by overriding
/// candidate slots and reading `balanceOf` back. A token not listed here is not funded, and a
/// check that would need it is skipped.
const TOKEN_SLOTS: [(Address, u64, u64); 2] = [
    (address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), 9, 10), // USDC
    (address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"), 9, 10), // cbBTC
];

/// What one fill check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProbeOutcome {
    Filled(FillReport),
    /// The proxy reverted, with the node's message.
    Reverted(String),
    /// The input is an ERC20 the check cannot fund (see [`TOKEN_SLOTS`]).
    Unfunded,
}

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
}

impl HanjiReader {
    pub(super) fn new(rpc_url: Url, deployment: Deployment, taker: Address) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            provider: RootProvider::new_http(rpc_url),
            deployment,
            taker,
            wiring: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reads `markets` at the latest block. A market whose calls revert, whose config names other
    /// tokens or another native side than the list, or whose wiring changed is left out, and the
    /// read logs how many with the first reason; it fails when every market was left out that way
    /// or none is served (see the module docs), and fails fatally when the node serves another
    /// chain. A paused market is left out without failing anything.
    pub(super) async fn read(&self, markets: &[Market]) -> Result<BookRead, FeedError> {
        self.resolve_wiring(markets).await?;
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
        // `aggregate` checked the answer count, so the chunks are whole and one per market.
        let (market_answers, _) = market_answers.as_chunks::<MARKET_CALLS>();
        for ((market, wiring), answers) in wired.iter().zip(market_answers) {
            match self.decode_market(*market, wiring, answers) {
                Ok(Some(market_read)) => read.markets.push(market_read),
                Ok(None) => {}
                Err(reason) => failures.push((market.orderbook, reason)),
            }
        }
        let Some((orderbook, reason)) = failures.first() else {
            return Ok(read);
        };
        if read.markets.is_empty() {
            return Err(FeedError::Parsing(format!(
                "Hanji: none of the {} markets read cleanly; {orderbook} failed with: {reason}",
                markets.len()
            )));
        }
        warn!(skipped = failures.len(), %orderbook, %reason, "skipping Hanji markets");
        Ok(read)
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
                Resolution::Unserved { .. } => None,
            })
            .collect()
    }

    /// Resolves, in one multicall, the wiring of every market not yet resolved or whose
    /// [`UNSERVED_RETRY`] has passed. A market whose getters revert, or that names no contract
    /// quoter, is left out of the reads until then.
    async fn resolve_wiring(&self, markets: &[Market]) -> Result<(), FeedError> {
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
            return Err(format!("its wiring changed from {wiring:?} to {found:?}"));
        }
        let config = Self::decode_config(market, config)?;
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

    /// Sends `probe` to the market's proxy at `block`, as the taker, and reports what the proxy
    /// filled. The taker's code is replaced by [`FORWARDER`] and it is given the input, so the
    /// proxy fills it as it fills the taker inside a route. A connection failure is an error; a
    /// revert is an outcome.
    pub(super) async fn probe(
        &self,
        block: u64,
        timestamp: u64,
        read: &MarketRead,
        probe: &Probe,
    ) -> Result<ProbeOutcome, FeedError> {
        let market = &read.market;
        let (token, native, scaling) = if probe.sell_base {
            (market.token_x, market.native == NativeSide::X, read.config.scaling_x)
        } else {
            (market.token_y, market.native == NativeSide::Y, read.config.scaling_y)
        };
        let amount = probe
            .units
            .checked_mul(scaling)
            .ok_or_else(|| FeedError::Parsing("Hanji fill check input overflows".into()))?;
        let mut overrides = StateOverride::default();
        overrides.insert(
            self.taker,
            AccountOverride::default()
                .with_code(FORWARDER)
                .with_balance(amount),
        );
        let value = if native {
            overrides.insert(PROBE_SENDER, AccountOverride::default().with_balance(amount));
            amount
        } else {
            let Some(funding) = erc20_funding(token, self.taker, market.orderbook, amount) else {
                return Ok(ProbeOutcome::Unfunded);
            };
            overrides.insert(token, funding);
            U256::ZERO
        };
        let units = u128::try_from(probe.units)
            .map_err(|e| FeedError::Parsing(format!("Hanji fill check input: {e}")))?;
        let expires = U256::from(timestamp) + U256::from(60);
        let call = if probe.sell_base {
            placeOrderCall {
                isAsk: true,
                quantity: units,
                price: MIN_PRICE,
                maxCommission: u128::MAX,
                marketOnly: true,
                postOnly: false,
                transferExecutedTokens: true,
                expires,
            }
            .abi_encode()
        } else {
            placeMarketOrderWithTargetValueCall {
                isAsk: false,
                targetTokenYValue: units,
                price: MAX_PRICE,
                maxCommission: u128::MAX,
                transferExecutedTokens: true,
                expires,
            }
            .abi_encode()
        };
        let mut input = market.orderbook.to_vec();
        input.extend(call);
        let request = TransactionRequest::default()
            .from(PROBE_SENDER)
            .to(self.taker)
            .value(value)
            .input(TransactionInput::new(input.into()));
        let answer = self
            .provider
            .call(request)
            .block(BlockId::number(block))
            .overrides(overrides)
            .await;
        let raw = match answer {
            Ok(raw) => raw,
            Err(RpcError::ErrorResp(error)) => {
                return Ok(ProbeOutcome::Reverted(format!(
                    "{} {}",
                    error.message,
                    error
                        .data
                        .as_ref()
                        .map_or("", |data| data.get())
                )));
            }
            Err(e) => return Err(self.connection_error(e)),
        };
        let report = if probe.sell_base {
            placeOrderCall::abi_decode_returns(&raw).map(|fill| FillReport {
                shares: U256::from(fill.executedShares),
                value: U256::from(fill.executedValue),
                fee: U256::from(fill.aggressiveFee),
            })
        } else {
            placeMarketOrderWithTargetValueCall::abi_decode_returns(&raw).map(|fill| FillReport {
                shares: U256::from(fill.executedShares),
                value: U256::from(fill.executedValue),
                fee: U256::from(fill.aggressiveFee),
            })
        };
        report
            .map(ProbeOutcome::Filled)
            .map_err(|e| FeedError::Parsing(format!("Hanji fill check answer: {e}")))
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
            .map_err(|e| self.connection_error(e))?;
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
    fn connection_error(&self, error: impl std::fmt::Display) -> FeedError {
        let error = error
            .to_string()
            .replace(&self.rpc_url, "<rpc url>");
        FeedError::Connection(format!("Hanji multicall: {error}"))
    }
}

/// The storage writes that give `owner` `amount` of `token` and let `spender` take it all, or
/// `None` for a token whose slots [`TOKEN_SLOTS`] does not list.
fn erc20_funding(
    token: Address,
    owner: Address,
    spender: Address,
    amount: U256,
) -> Option<AccountOverride> {
    let (_, balance_slot, allowance_slot) = TOKEN_SLOTS
        .iter()
        .find(|(listed, _, _)| *listed == token)?;
    let slot = |key: B256, slot: u64| keccak256((key, U256::from(slot)).abi_encode());
    let balance = slot(owner.into_word(), *balance_slot);
    let allowance =
        keccak256((spender.into_word(), slot(owner.into_word(), *allowance_slot)).abi_encode());
    Some(
        AccountOverride::default()
            .with_state_diff([(balance, B256::from(amount)), (allowance, B256::from(U256::MAX))]),
    )
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
