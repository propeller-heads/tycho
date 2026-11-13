//! A JSON-RPC node for the feed's tests, serving one factory, its pools, their hooks, their price
//! feed and their lending router from Rust instead of a chain. It answers the sampler's call with
//! walks computed from its pools as the sampler would walk them, its seeds and then the probe
//! above a tier-labelled top, with none of its bisection: `sampler_evm` runs the sampler's own
//! bytes.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use alloy::{
    primitives::{Address, FixedBytes, B256, U256},
    sol_types::SolCall,
};
use serde_json::{json, Value};

use crate::{
    pamm::protocols::flamm::{
        ladder::{Probe, ProbeStatus, ABOVE_SHIFT, ANSWER_BITS, DUST_OUT},
        reader::{
            capabilitiesCall, hooksCall, isListedFrontCall, isPoolCall, laneCall, minimumDebtCall,
            minimumDebtReturn, paramsCall, peekUsdCall, positionCall, priceFeedCall, routerCall,
            tokenConfigCall, tokenCountCall, totalAssetsCall, tradablePairsCall, trade_bit,
            venueActionsCall, venueCall, venueCountCall, FlammSampler, HookSet, TokenConfig,
            VenueView, LEV_STATE_SELECTOR, QUOTE_PAIR_SELECTOR, SAMPLER_RUNTIME, SAMPLER_TAG,
        },
    },
    snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer, MockResponse},
};

pub(super) const CHAIN_ID: u64 = 8453;
pub(super) const BLOCK: u64 = 1_000;
pub(super) const FACTORY: Address = Address::repeat_byte(0xfa);
/// An address with nothing deployed at it.
pub(super) const NO_CODE: Address = Address::repeat_byte(0xee);
pub(super) const FEED: Address = Address::repeat_byte(0xfe);
/// Another deployment of [`FEED`]'s code.
pub(super) const TWIN_FEED: Address = Address::repeat_byte(0xfd);
pub(super) const ROGUE_FEED: Address = Address::repeat_byte(0xfc);
pub(super) const ROUTER: Address = Address::repeat_byte(0xa0);
pub(super) const HOOK: Address = Address::repeat_byte(0x40);
pub(super) const ROGUE_HOOK: Address = Address::repeat_byte(0x41);
/// A hook of a class of its own that the factory lists as a front.
pub(super) const FRONT: Address = Address::repeat_byte(0x42);
pub(super) const UNLISTED_FRONT: Address = Address::repeat_byte(0x43);
/// The runtime code of [`HOOK`].
pub(super) const HOOK_CODE: [u8; 3] = [0x60, 0x01, 0x00];
/// The runtime code of [`FEED`] and [`TWIN_FEED`].
pub(super) const FEED_CODE: [u8; 3] = [0x60, 0x02, 0x00];
/// The label of an answer the curve priced, which `quotePair` reports as the curve's fee: 0.3 %.
pub(super) const CURVE_FEE: u64 = 3_000_000_000_000_000;
/// The labels of [`HOOK`]'s two leverage routes, its row's spreads: 1.75 % and 1.3 %.
pub(super) const TIER_UP: u64 = 17_500_000_000_000_000;
pub(super) const TIER_DOWN: u64 = 13_000_000_000_000_000;
/// Token 1's price band, and so the hook pair's: 8 %.
pub(super) const BAND: u64 = 80_000_000_000_000_000;
/// Words 27-34 of [`HOOK`]'s `params()`: its spreads, caps of .5 % and 2 %, a ceiling of 2.2, a
/// band of 5 %, no feed gate and a floor of 1.7.
const ROW: [u64; 8] = [
    TIER_UP,
    TIER_DOWN,
    5_000_000_000_000_000,
    20_000_000_000_000_000,
    2_200_000_000_000_000_000,
    50_000_000_000_000_000,
    0,
    1_700_000_000_000_000_000,
];
const WAD: u64 = 1_000_000_000_000_000_000;
/// The selector the Tycho router's own code reverts with for a call it has no entry for: its
/// fallback takes the call for an executor's callback, from an executor it has not approved
/// (`crates/tycho-execution/contracts/src/TychoRouterV3.sol:1135-1140`,
/// `crates/tycho-execution/contracts/src/Dispatcher.sol:272-278`).
const ROUTER_REVERT: &str = "0xa6cdd2a5";
/// What a sampler call spends before its first call, and each call of a pool that needs no more.
const ENTRY_GAS: u64 = 50_000;
const CALL_GAS: u64 = 150_000;
/// The gas a node gives a call that names none.
const NODE_GAS: u64 = 50_000_000;

/// The runtime code at `address`: each hook or feed but the same-code pairs has its own.
pub(super) fn code_at(address: Address) -> Vec<u8> {
    match address {
        a if a == HOOK => HOOK_CODE.to_vec(),
        a if a == FEED || a == TWIN_FEED => FEED_CODE.to_vec(),
        a if a.is_zero() || a == NO_CODE => Vec::new(),
        a => vec![0x60, a[0], 0x00],
    }
}

#[derive(Clone, Debug)]
pub(super) struct FakeToken {
    pub address: Address,
    pub decimals: u8,
    /// USD price, WAD; `None` where the feed does not price it.
    pub usd_wad: Option<U256>,
    /// What the pool holds of it, base units; it lends, posts and owes none.
    pub held: U256,
}

/// What a hook answers: its row, the curve's fee and the lever-up's top bid.
#[derive(Clone, Debug)]
pub(super) struct FakeHook {
    /// The 35 words of `params()`; `None` where it reverts.
    pub params: Option<[U256; 35]>,
    /// Word 3 of `quotePair`'s answer.
    pub curve_fee: U256,
    /// Word 6 of `levState`'s answer.
    pub bid: U256,
}

impl FakeHook {
    /// A hook whose row allows no leverage.
    fn plain() -> Self {
        Self { params: Some([U256::ZERO; 35]), curve_fee: U256::from(CURVE_FEE), bid: U256::ZERO }
    }

    /// A hook whose row allows leverage, [`ROW`], with a top bid of 1.
    pub(super) fn levered() -> Self {
        let mut params = [U256::ZERO; 35];
        for (word, value) in params[27..].iter_mut().zip(ROW) {
            *word = U256::from(value);
        }
        Self { params: Some(params), bid: U256::from(WAD), ..Self::plain() }
    }

    /// Its answer to `quotePair` or `levState` with a request of 26 words.
    fn answer(&self, selector: [u8; 4], words: usize) -> Option<Vec<U256>> {
        if words != 26 {
            return None;
        }
        match selector {
            s if s == QUOTE_PAIR_SELECTOR => {
                let mut fill = vec![U256::ZERO; 7];
                fill[3] = self.curve_fee;
                Some(fill)
            }
            s if s == LEV_STATE_SELECTOR => {
                let mut state = vec![U256::ZERO; 8];
                state[6] = self.bid;
                Some(state)
            }
            _ => None,
        }
    }
}

/// The hooks the node knows: [`HOOK`] with a leverage row, the others with none.
fn hooks() -> HashMap<Address, FakeHook> {
    HashMap::from([
        (HOOK, FakeHook::levered()),
        (ROGUE_HOOK, FakeHook::plain()),
        (FRONT, FakeHook::plain()),
        (UNLISTED_FRONT, FakeHook::plain()),
    ])
}

/// What a pool's node gets wrong, if anything. The sampler's faults are the node's answer to the
/// sampler call for that pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Fault {
    #[default]
    None,
    /// Every view reverts.
    Views,
    /// The node runs the sampler call out of gas.
    OutOfGas,
    /// `totalAssets` reverts, as while a feed the pool marks a position at is unchecked.
    Nav,
    /// The node answers every view with an error of its own.
    ViewsUnanswered,
    /// The node answers the sampler call with an error of its own.
    QuotesUnanswered,
    /// `pairRequest` reverts.
    RequestRefused,
    /// The node drops the state override, so the router's own code runs, and reverts.
    OverrideDropped,
    /// The node refuses a call with a state override.
    OverrideRefused,
    /// The node answers with a tag other than the sampler's.
    BadTag,
    /// The node turns the call away, in one of a rate limit's forms.
    RateLimited(Limit),
    /// The node takes this long to answer the call.
    SlowNode(Duration),
    /// Each `previewSwap` needs this much gas: a cap it spends more than 7/8 of runs short.
    Starved(u64),
    /// Each `previewSwap` paying more than the pool holds of its output needs this much gas, as
    /// one that borrows reads its venue's markets.
    StarvedPastCustody(u64),
    /// The node answers from the block before the one asked for.
    OlderBlock,
    /// The node does not have the block asked for.
    NoBlock,
    /// `previewSwap` pays nothing for what it uses.
    Malformed,
}

/// A rate limit's form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Limit {
    /// An error with this code, worded with no limit.
    Code(i64),
    /// This HTTP status.
    Http(u16),
    /// An error of a generic code with this wording.
    Wording(&'static str),
    /// A null result.
    Null,
}

/// A pool that sells either of its first two tokens for the other at a fixed rate, less a fee, up
/// to a cap on the input it uses, and lets every pair trade.
#[derive(Clone, Debug)]
pub(super) struct FakePool {
    pub address: Address,
    pub tokens: Vec<FakeToken>,
    /// Output per input, as `(numerator, denominator)` in base units, for token 0 in and token 1
    /// in.
    pub rates: [(u128, u128); 2],
    /// The most input a quote uses, per token in; past it the quote is clipped.
    pub caps: [U256; 2],
    pub lane_on: bool,
    pub hook: Address,
    pub feed: Address,
    pub fault: Fault,
    pub venue_kinds: Vec<u8>,
    /// The status every venue of the pool reports: zero while it is live.
    pub venue_status: u8,
    /// What every venue of the pool lets it do now (`venueActions`), or `None` where the router
    /// refuses to say.
    pub venue_actions: Option<u8>,
    /// What the account behind every venue of the pool declares (`capabilities()`), or `None`
    /// where it reverts.
    pub venue_capabilities: Option<u64>,
    /// What that account's `minimumDebt(id)` answers, or `None` where it reverts.
    pub venue_minimum: Option<(bool, U256)>,
    /// Per token in, the label of a quote that uses at least each input, in increasing input:
    /// [`CURVE_FEE`] under the first.
    pub labels: [Vec<(U256, U256)>; 2],
    /// Token 1's `swapPriceBandWad`; token 0's is zero.
    pub band_wad: u64,
}

impl FakePool {
    fn token_index(&self, token: Address) -> Option<usize> {
        self.tokens
            .iter()
            .position(|t| t.address == token)
    }

    /// What the pool holds of `token`.
    fn held(&self, token: Address) -> U256 {
        self.token_index(token)
            .map_or(U256::ZERO, |k| self.tokens[k].held)
    }

    /// The input used, the output and the label of a quote of `amount`.
    pub(super) fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount: U256,
    ) -> Option<(U256, U256, U256)> {
        let (i, j) = (self.token_index(token_in)?, self.token_index(token_out)?);
        if i == j || amount.is_zero() {
            return None;
        }
        let used = amount.min(*self.caps.get(i)?);
        let (num, den) = *self.rates.get(i)?;
        let out = used * U256::from(num) / U256::from(den);
        let label = self
            .labels
            .get(i)?
            .iter()
            .rev()
            .find(|(from, _)| used >= *from)
            .map_or_else(|| U256::from(CURVE_FEE), |(_, label)| *label);
        (!out.is_zero()).then_some((used, out, label))
    }

    /// The 26 words of the pool's `pairRequest` for `amount` of `token_in` from `sender`: the
    /// sender, the tokens and their indices, the hook pair's band, the tokens' USD prices, the
    /// input and the legs, of which a fake pool only holds.
    fn request(
        &self,
        sender: Option<Address>,
        token_in: Address,
        token_out: Address,
        amount: U256,
    ) -> Option<[U256; 26]> {
        let (i, j) = (self.token_index(token_in)?, self.token_index(token_out)?);
        if i == j || amount.is_zero() {
            return None;
        }
        let word = |address: Address| U256::from_be_slice(address.as_slice());
        let mut words = [U256::ZERO; 26];
        words[0] = word(sender.unwrap_or_default());
        words[1] = word(token_in);
        words[2] = word(token_out);
        words[3] = U256::from(i);
        words[4] = U256::from(j);
        words[10] = U256::from(self.band_wad);
        words[11] = self.tokens[i]
            .usd_wad
            .unwrap_or_default();
        words[12] = self.tokens[j]
            .usd_wad
            .unwrap_or_default();
        words[13] = amount;
        words[15] = self.tokens[i].held;
        words[19] = self.tokens[j].held;
        Some(words)
    }
}

/// A `pairRequest` the node answered.
#[derive(Clone, Debug)]
pub(super) struct RequestSeen {
    pub from: Option<Address>,
    pub token_in: Address,
    pub amount: U256,
    pub words: [U256; 26],
}

/// A `quotePair` or `levState` the node answered, with the request words it carried.
#[derive(Clone, Debug)]
pub(super) struct HookReadSeen {
    pub hook: Address,
    pub selector: [u8; 4],
    pub from: Option<Address>,
    pub words: Vec<U256>,
}

/// A sampler call the node was sent.
#[derive(Clone, Debug)]
pub(super) struct SampleSeen {
    /// When it reached the node.
    pub at: std::time::Instant,
    /// Where the call went, and so where the sampler's code runs.
    pub to: Address,
    pub from: Option<Address>,
    pub block: Option<u64>,
    pub gas: Option<u64>,
    /// The code the state override puts at `to`.
    pub code: Option<Vec<u8>>,
    pub call: FlammSampler::sampleCall,
}

/// What the node saw: the sender, the pool, the tokens and the input of every `previewSwap` the
/// sampler made, every storage read, the code reads, the views by address, the requests and the
/// hooks' answers, and the sampler calls.
#[derive(Default)]
pub(super) struct Seen {
    pub quote_senders: Vec<Option<Address>>,
    pub quoted_pools: Vec<Address>,
    pub quoted_pairs: Vec<(Address, Address)>,
    pub quoted_amounts: Vec<U256>,
    pub storage_reads: usize,
    pub code_reads: HashMap<Address, usize>,
    pub views: HashMap<Address, usize>,
    pub params_reads: HashMap<Address, usize>,
    /// Calls to a hook of any other view than `params()`, sent to it directly.
    pub other_hook_calls: usize,
    pub pair_requests: Vec<RequestSeen>,
    pub hook_reads: Vec<HookReadSeen>,
    pub samples: Vec<SampleSeen>,
}

impl Seen {
    /// The sampler calls for `pool`.
    pub(super) fn samples_of(&self, pool: Address) -> Vec<&SampleSeen> {
        self.samples
            .iter()
            .filter(|sample| sample.call.pool == pool)
            .collect()
    }
}

/// The chain the node answers from; a test may change it while the node runs.
#[derive(Default)]
pub(super) struct FakeChain {
    /// Every pool, in the order the factory created them.
    pub pools: Vec<FakePool>,
    /// The hooks, by address.
    pub hooks: HashMap<Address, FakeHook>,
    /// Addresses in the factory's array that `isPool` denies.
    pub not_pools: HashSet<Address>,
    /// How far past [`BLOCK`] the head is.
    pub blocks_ahead: u64,
    /// The node answers every read of the factory with an error of its own.
    pub factory_down: bool,
    /// How long the node takes to answer a storage read.
    pub storage_delay: Duration,
    /// Whether the node fails the next sampler call with an error of its own.
    pub flaky_quote: AtomicBool,
    /// The most gas the node gives a call, whatever it asks.
    pub gas_cap: Option<u64>,
}

impl FakeChain {
    /// The fault of the pool `call` runs the sampler for, if it is a sampler call.
    fn sampler_fault(&self, call: &Value) -> Option<Fault> {
        if call["method"].as_str() != Some("eth_call") {
            return None;
        }
        let data = alloy::hex::decode(input(&call["params"][0])).ok()?;
        let call = FlammSampler::sampleCall::abi_decode(&data).ok()?;
        self.pool(call.pool)
            .map(|pool| pool.fault)
    }

    /// How long the node takes to answer `call`.
    fn delay(&self, call: &Value) -> Duration {
        if call["method"].as_str() == Some("eth_getStorageAt") {
            return self.storage_delay;
        }
        match self.sampler_fault(call) {
            Some(Fault::SlowNode(delay)) => delay,
            Some(_) | None => Duration::ZERO,
        }
    }

    /// The HTTP status the node answers `call` with.
    fn status(&self, call: &Value) -> &'static str {
        match self.sampler_fault(call) {
            Some(Fault::RateLimited(Limit::Http(413))) => "413 Payload Too Large",
            Some(Fault::RateLimited(Limit::Http(429))) => "429 Too Many Requests",
            Some(Fault::RateLimited(Limit::Http(503))) => "503 Service Unavailable",
            Some(Fault::RateLimited(Limit::Http(_))) => "500 Internal Server Error",
            Some(_) | None => "200 OK",
        }
    }

    fn pool(&self, address: Address) -> Option<&FakePool> {
        self.pools
            .iter()
            .find(|p| p.address == address)
    }
}

pub(super) struct FakeNode {
    pub server: MockHttpServer,
    pub seen: Arc<Mutex<Seen>>,
    pub chain: Arc<Mutex<FakeChain>>,
}

fn revert() -> Value {
    json!({"code": 3, "message": "execution reverted", "data": "0x"})
}

/// An error of the node's own, which says nothing of the called code.
fn unavailable() -> Value {
    json!({"code": -32603, "message": "internal error"})
}

fn revert_because(reason: impl std::fmt::Display) -> Value {
    json!({"code": 3, "message": format!("execution reverted: {reason}"), "data": "0x"})
}

fn hex(data: &[u8]) -> String {
    format!("0x{}", alloy::hex::encode(data))
}

fn word(address: &str) -> Address {
    address.parse().unwrap_or_default()
}

/// A call's data, as hex.
fn input(tx: &Value) -> &str {
    tx["input"]
        .as_str()
        .or_else(|| tx["data"].as_str())
        .unwrap_or_default()
}

/// A hex quantity.
fn quantity(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

impl FakeNode {
    pub(super) async fn spawn(chain_id: u64, timestamp: u64, pools: Vec<FakePool>) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let chain =
            Arc::new(Mutex::new(FakeChain { pools, hooks: hooks(), ..FakeChain::default() }));
        let (recorded, state) = (Arc::clone(&seen), Arc::clone(&chain));
        let server = spawn_http_server(move |request| {
            let body: Value = serde_json::from_str(&request.body).ok()?;
            let state = state.lock().ok()?;
            let answer = |call: &Value| -> Value {
                let id = call["id"].clone();
                let result = Self::answer(call, chain_id, timestamp, &state, &recorded);
                match result {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                }
            };
            let reply = match &body {
                Value::Array(calls) => Value::Array(calls.iter().map(answer).collect()),
                call @ (Value::Null |
                Value::Bool(_) |
                Value::Number(_) |
                Value::String(_) |
                Value::Object(_)) => answer(call),
            };
            let (status, delay) = (state.status(&body), state.delay(&body));
            Some(MockResponse { status, body: reply.to_string(), delay })
        })
        .await;
        FakeNode { server, seen, chain }
    }

    fn answer(
        call: &Value,
        chain_id: u64,
        timestamp: u64,
        chain: &FakeChain,
        seen: &Mutex<Seen>,
    ) -> Result<Value, Value> {
        let params = &call["params"];
        match call["method"]
            .as_str()
            .unwrap_or_default()
        {
            "eth_chainId" => Ok(json!(format!("{chain_id:#x}"))),
            "eth_getBlockByNumber" => Ok(block(timestamp, BLOCK + chain.blocks_ahead)),
            "eth_getCode" => {
                let address = word(params[0].as_str().unwrap_or_default());
                *seen
                    .lock()
                    .map_err(revert_because)?
                    .code_reads
                    .entry(address)
                    .or_default() += 1;
                Ok(json!(hex(&code_at(address))))
            }
            "eth_getStorageAt" => {
                let address = word(params[0].as_str().unwrap_or_default());
                let slot: U256 = params[1]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .map_err(revert_because)?;
                seen.lock()
                    .map_err(revert_because)?
                    .storage_reads += 1;
                if chain.factory_down {
                    return Err(unavailable());
                }
                Ok(json!(hex(&Self::storage(address, slot, chain).to_be_bytes::<32>())))
            }
            "eth_call" => {
                let tx = &params[0];
                let to = word(tx["to"].as_str().unwrap_or_default());
                let data = alloy::hex::decode(input(tx)).map_err(revert_because)?;
                if data.starts_with(&FlammSampler::sampleCall::SELECTOR) {
                    return Self::sampler_call(params, to, &data, chain, seen);
                }
                Self::eth_call(to, &data, chain, seen).map(|out| json!(hex(&out)))
            }
            method => Err(json!({"code": -32601, "message": format!("no {method}")})),
        }
    }

    /// The factory's storage, as Solidity lays out `_pools`, its second slot; zero elsewhere.
    fn storage(address: Address, slot: U256, chain: &FakeChain) -> U256 {
        let first_entry = U256::from_be_slice(&alloy::hex!(
            "b10e2d527612073b26eecdfd717e6a320cf44b4afac2b0732d9fcbe2b7fa0cf6"
        ));
        if address != FACTORY {
            return U256::ZERO;
        }
        if slot == U256::from(1) {
            return U256::from(chain.pools.len());
        }
        slot.checked_sub(first_entry)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| chain.pools.get(index))
            .map_or(U256::ZERO, |pool| U256::from_be_slice(pool.address.as_slice()))
    }

    /// A view's answer. Quotes and requests are the sampler's to ask, so a direct call of one
    /// reverts, as an unknown selector does.
    fn eth_call(
        to: Address,
        data: &[u8],
        chain: &FakeChain,
        seen: &Mutex<Seen>,
    ) -> Result<Vec<u8>, Value> {
        let selector: [u8; 4] = data
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(revert)?;
        if to == FACTORY && chain.factory_down {
            return Err(unavailable());
        }
        if to == FACTORY && selector == isPoolCall::SELECTOR {
            let call = isPoolCall::abi_decode(data).map_err(revert_because)?;
            let created = chain.pool(call.pool).is_some() && !chain.not_pools.contains(&call.pool);
            return Ok(isPoolCall::abi_encode_returns(&created));
        }
        if to == FACTORY && selector == isListedFrontCall::SELECTOR {
            let call = isListedFrontCall::abi_decode(data).map_err(revert_because)?;
            return Ok(isListedFrontCall::abi_encode_returns(&(call.front == FRONT)));
        }
        if selector == peekUsdCall::SELECTOR {
            let call = peekUsdCall::abi_decode(data).map_err(revert_because)?;
            let price = chain
                .pools
                .iter()
                .flat_map(|p| p.tokens.iter())
                .find(|t| t.address == call.token)
                .and_then(|t| t.usd_wad);
            return Ok(peekUsdCall::abi_encode_returns(
                &crate::pamm::protocols::flamm::reader::peekUsdReturn {
                    ok: price.is_some(),
                    usdWad: price.unwrap_or_default(),
                    observedAt: alloy::primitives::aliases::U48::from(1u64),
                },
            ));
        }
        if to == ROUTER && selector == venueCountCall::SELECTOR {
            let call = venueCountCall::abi_decode(data).map_err(revert_because)?;
            let count = chain
                .pool(call.pool)
                .map_or(0, |p| p.venue_kinds.len());
            return Ok(venueCountCall::abi_encode_returns(&U256::from(count)));
        }
        if to == ROUTER && selector == venueActionsCall::SELECTOR {
            let call = venueActionsCall::abi_decode(data).map_err(revert_because)?;
            let actions = chain
                .pool(call.pool)
                .filter(|p| usize::from(call.id) < p.venue_kinds.len())
                .and_then(|p| p.venue_actions)
                .ok_or_else(revert)?;
            return Ok(venueActionsCall::abi_encode_returns(&actions));
        }
        if to == ROUTER && selector == venueCall::SELECTOR {
            let call = venueCall::abi_decode(data).map_err(revert_because)?;
            let pool = chain
                .pool(call.pool)
                .ok_or_else(revert)?;
            let kind = pool
                .venue_kinds
                .get(usize::from(call.id))
                .copied()
                .ok_or_else(revert)?;
            return Ok(venueCall::abi_encode_returns(&VenueView {
                account: account_of(pool.address),
                id: B256::ZERO,
                kind,
                pair: 0,
                coll: 0,
                debt: 1,
                lltvWad: 0,
                borrowEnabled: true,
                supplyEnabled: true,
                status: pool.venue_status,
                debtCap: 0,
                supplyCap: 0,
                maxBorrowRateWad: 0,
            }));
        }
        if let Some(pool) = chain
            .pools
            .iter()
            .find(|p| account_of(p.address) == to)
        {
            if selector == capabilitiesCall::SELECTOR {
                let bits = pool
                    .venue_capabilities
                    .ok_or_else(revert)?;
                return Ok(capabilitiesCall::abi_encode_returns(&U256::from(bits)));
            }
            if selector == minimumDebtCall::SELECTOR {
                let (ok, floor) = pool.venue_minimum.ok_or_else(revert)?;
                return Ok(minimumDebtCall::abi_encode_returns(&minimumDebtReturn { ok, floor }));
            }
            return Err(revert());
        }
        if let Some(hook) = chain.hooks.get(&to) {
            return Self::hook_call(hook, to, selector, seen);
        }
        let pool = chain.pool(to).ok_or_else(revert)?;
        *seen
            .lock()
            .map_err(revert_because)?
            .views
            .entry(to)
            .or_default() += 1;
        match pool.fault {
            Fault::Views => return Err(revert()),
            Fault::ViewsUnanswered => return Err(unavailable()),
            Fault::None |
            Fault::OutOfGas |
            Fault::Nav |
            Fault::QuotesUnanswered |
            Fault::RequestRefused |
            Fault::OverrideDropped |
            Fault::OverrideRefused |
            Fault::BadTag |
            Fault::RateLimited(_) |
            Fault::SlowNode(_) |
            Fault::Starved(_) |
            Fault::StarvedPastCustody(_) |
            Fault::OlderBlock |
            Fault::NoBlock |
            Fault::Malformed => {}
        }
        match selector {
            s if s == tokenCountCall::SELECTOR => {
                Ok(tokenCountCall::abi_encode_returns(&U256::from(pool.tokens.len())))
            }
            s if s == tradablePairsCall::SELECTOR => {
                let n = pool.tokens.len();
                let every_way = (0..n)
                    .flat_map(|i| (0..n).map(move |j| (i, j)))
                    .filter(|(i, j)| i != j)
                    .fold(U256::ZERO, |mask, (i, j)| mask | trade_bit(i, j));
                Ok(tradablePairsCall::abi_encode_returns(
                    &crate::pamm::protocols::flamm::reader::tradablePairsReturn {
                        tradable: every_way,
                        hookPairs: every_way,
                        operatorPairs: every_way,
                        pendingPairs: U256::ZERO,
                        executableAt: alloy::primitives::aliases::U48::ZERO,
                    },
                ))
            }
            s if s == hooksCall::SELECTOR => Ok(hooksCall::abi_encode_returns(&HookSet {
                hook: pool.hook,
                controller: Address::ZERO,
            })),
            s if s == laneCall::SELECTOR => {
                let config = if pool.lane_on { U256::from(1) << 97 } else { U256::ZERO };
                Ok(laneCall::abi_encode_returns(
                    &crate::pamm::protocols::flamm::reader::laneReturn {
                        laneWord: U256::ZERO,
                        laneConfig: config,
                        pending: U256::ZERO,
                    },
                ))
            }
            s if s == priceFeedCall::SELECTOR => Ok(priceFeedCall::abi_encode_returns(&pool.feed)),
            s if s == routerCall::SELECTOR => Ok(routerCall::abi_encode_returns(&ROUTER)),
            s if s == totalAssetsCall::SELECTOR => {
                if pool.fault == Fault::Nav {
                    return Err(revert());
                }
                Ok(totalAssetsCall::abi_encode_returns(&U256::from(10u64).pow(U256::from(30))))
            }
            s if s == positionCall::SELECTOR => {
                let call = positionCall::abi_decode(data).map_err(revert_because)?;
                let token = pool
                    .tokens
                    .get(usize::from(call.t))
                    .ok_or_else(revert)?;
                Ok(positionCall::abi_encode_returns(
                    &crate::pamm::protocols::flamm::reader::positionReturn {
                        held: token.held,
                        supplied: U256::ZERO,
                        posted: U256::ZERO,
                        owed: U256::ZERO,
                    },
                ))
            }
            s if s == tokenConfigCall::SELECTOR => {
                let call = tokenConfigCall::abi_decode(data).map_err(revert_because)?;
                let token = pool
                    .tokens
                    .get(usize::from(call.t))
                    .ok_or_else(revert)?;
                Ok(tokenConfigCall::abi_encode_returns(&TokenConfig {
                    token: token.address,
                    decimals: token.decimals,
                    swapPriceBandWad: if call.t == 1 { pool.band_wad } else { 0 },
                    feeFloorWad: 0,
                    maxSwapNotional: U256::ZERO,
                    reserveTarget: U256::ZERO,
                    minSupplyBatch: U256::ZERO,
                    held: token.held,
                    partner: 0,
                    pairId: 0,
                    debtFree: true,
                }))
            }
            _ => Err(revert()),
        }
    }

    /// A hook's answer to `params()`, the one view of it discovery reads.
    fn hook_call(
        hook: &FakeHook,
        to: Address,
        selector: [u8; 4],
        seen: &Mutex<Seen>,
    ) -> Result<Vec<u8>, Value> {
        if selector != paramsCall::SELECTOR {
            seen.lock()
                .map_err(revert_because)?
                .other_hook_calls += 1;
            return Err(revert());
        }
        *seen
            .lock()
            .map_err(revert_because)?
            .params_reads
            .entry(to)
            .or_default() += 1;
        let params = hook.params.ok_or_else(revert)?;
        Ok(paramsCall::abi_encode_returns(&params))
    }

    /// The node's answer to a sampler call sent to `to`: the sampler's walks where the state
    /// override puts its code there, else what `to`'s own code answers, which for the router is a
    /// revert.
    fn sampler_call(
        params: &Value,
        to: Address,
        data: &[u8],
        chain: &FakeChain,
        seen: &Mutex<Seen>,
    ) -> Result<Value, Value> {
        let tx = &params[0];
        let call = FlammSampler::sampleCall::abi_decode(data).map_err(revert_because)?;
        let code = params[2]
            .as_object()
            .and_then(|accounts| {
                accounts
                    .iter()
                    .find(|(address, _)| word(address) == to)
            })
            .and_then(|(_, account)| account["code"].as_str())
            .and_then(|code| alloy::hex::decode(code).ok());
        let block = params[1]
            .as_str()
            .and_then(quantity)
            .unwrap_or(BLOCK + chain.blocks_ahead);
        let gas = tx["gas"].as_str().and_then(quantity);
        let mut seen = seen.lock().map_err(revert_because)?;
        seen.samples.push(SampleSeen {
            at: std::time::Instant::now(),
            to,
            from: tx["from"].as_str().map(word),
            block: params[1].as_str().and_then(quantity),
            gas,
            code: code.clone(),
            call: call.clone(),
        });
        let pool = chain
            .pool(call.pool)
            .ok_or_else(revert)?;
        let ran = match pool.fault {
            Fault::OverrideDropped => false,
            Fault::OverrideRefused => {
                return Err(json!({
                    "code": -32602,
                    "message": "invalid argument 2: state override is not supported"
                }));
            }
            Fault::OutOfGas => return Err(json!({"code": -32000, "message": "out of gas"})),
            Fault::QuotesUnanswered => return Err(unavailable()),
            Fault::RateLimited(Limit::Code(code)) => {
                return Err(json!({"code": code, "message": "request refused"}));
            }
            Fault::RateLimited(Limit::Http(_)) => return Err(unavailable()),
            Fault::RateLimited(Limit::Wording(wording)) => {
                return Err(json!({"code": -32000, "message": wording}));
            }
            Fault::RateLimited(Limit::Null) => return Ok(Value::Null),
            Fault::NoBlock => return Err(json!({"code": -32000, "message": "header not found"})),
            Fault::None |
            Fault::Views |
            Fault::Nav |
            Fault::ViewsUnanswered |
            Fault::RequestRefused |
            Fault::BadTag |
            Fault::SlowNode(_) |
            Fault::Starved(_) |
            Fault::StarvedPastCustody(_) |
            Fault::OlderBlock |
            Fault::Malformed => code.as_deref() == Some(SAMPLER_RUNTIME),
        };
        if chain
            .flaky_quote
            .swap(false, Ordering::SeqCst)
        {
            return Err(unavailable());
        }
        if !ran {
            return Err(json!({"code": 3, "message": "execution reverted", "data": ROUTER_REVERT}));
        }
        let gas = gas
            .unwrap_or(NODE_GAS)
            .min(chain.gas_cap.unwrap_or(u64::MAX));
        let walks = chain.sample(to, &call, pool, gas, &mut seen);
        let tag = if pool.fault == Fault::BadTag { [0xba, 0xd0, 0x7a, 0x60] } else { SAMPLER_TAG };
        let block = if pool.fault == Fault::OlderBlock { block.saturating_sub(1) } else { block };
        let answer = FlammSampler::sampleReturn {
            tag: FixedBytes(tag),
            blockNumber: U256::from(block),
            walks,
        };
        Ok(json!(hex(&FlammSampler::sampleCall::abi_encode_returns(&answer))))
    }
}

impl FakeChain {
    /// `call`'s walks of `pool`, as the sampler at `taker` with `gas` would take them: its seeds,
    /// round robin, then each walk's request read and its probe above a tier-labelled top, each
    /// call to the pool or hook behind the sampler's gas gate and capped.
    fn sample(
        &self,
        taker: Address,
        call: &FlammSampler::sampleCall,
        pool: &FakePool,
        gas: u64,
        seen: &mut Seen,
    ) -> Vec<FlammSampler::Walk> {
        let cap = u64::try_from(call.limits.probeGas).unwrap_or(u64::MAX);
        let reserve = u64::try_from(call.limits.reserveGas).unwrap_or(u64::MAX);
        let mut run = Run {
            taker,
            cap,
            gate: (cap.saturating_mul(64) / 63).saturating_add(reserve),
            left: gas.saturating_sub(ENTRY_GAS),
            seen,
        };
        let mut walks: Vec<Walking<'_>> = call
            .asks
            .iter()
            .map(Walking::new)
            .collect();
        while walks.iter().any(|walk| walk.seeding) {
            for walk in &mut walks {
                walk.seed(pool, &mut run);
            }
        }
        for walk in &mut walks {
            walk.read_request(self, pool, &mut run);
        }
        for walk in &mut walks {
            walk.probe_above(pool, &mut run);
        }
        walks
            .into_iter()
            .map(Walking::finish)
            .collect()
    }
}

/// A sampler call's gas as the fake node runs it, and what it saw of the calls.
struct Run<'a> {
    taker: Address,
    cap: u64,
    /// The gas a call needs left before it is made.
    gate: u64,
    left: u64,
    seen: &'a mut Seen,
}

impl Run<'_> {
    /// Spends what a call needs, up to the cap, where the gate lets it be made.
    fn spend(&mut self, need: u64) -> Option<u64> {
        if self.left < self.gate {
            return None;
        }
        let spent = need.min(self.cap);
        self.left = self.left.saturating_sub(spent);
        Some(spent)
    }

    /// `pool`'s answer to `amount` in `ask`'s direction, and the gas it spent; `None` where the
    /// gate stops it.
    fn probe(
        &mut self,
        pool: &FakePool,
        ask: &FlammSampler::Ask,
        amount: U256,
    ) -> Option<(Probe, u64)> {
        let quote = pool.quote(ask.tokenIn, ask.tokenOut, amount);
        let past_custody = quote.is_some_and(|(_, out, _)| out > pool.held(ask.tokenOut));
        let need = if let Fault::Starved(need) = pool.fault {
            need
        } else if let (Fault::StarvedPastCustody(need), true) = (pool.fault, past_custody) {
            need
        } else {
            CALL_GAS
        };
        let spent = self.spend(need)?;
        self.seen
            .quote_senders
            .push(Some(self.taker));
        self.seen
            .quoted_pools
            .push(pool.address);
        self.seen
            .quoted_pairs
            .push((ask.tokenIn, ask.tokenOut));
        self.seen.quoted_amounts.push(amount);
        let probe = if spent.saturating_mul(8) > self.cap.saturating_mul(7) {
            Probe {
                ask: amount,
                used: U256::ZERO,
                out: U256::ZERO,
                fee_wad: U256::ZERO,
                status: ProbeStatus::Starved,
            }
        } else if pool.fault == Fault::Malformed {
            Probe::classify(amount, Some((amount, U256::ZERO, U256::from(CURVE_FEE))))
        } else {
            Probe::classify(amount, quote)
        };
        Some((probe, spent))
    }
}

/// One walk as the fake node takes it.
struct Walking<'a> {
    ask: &'a FlammSampler::Ask,
    /// Each probe and the gas it spent, in increasing ask.
    probes: Vec<(Probe, u64)>,
    /// The index of the next seed.
    next: usize,
    seeding: bool,
    /// The output and label of the last priced answer paying at least dust.
    last: Option<(U256, U256)>,
    /// The labels of the priced answers paying at least dust.
    labels: Vec<U256>,
    /// The walk's words, its probes left out until it ends.
    words: FlammSampler::Walk,
}

impl<'a> Walking<'a> {
    fn new(ask: &'a FlammSampler::Ask) -> Self {
        Self {
            ask,
            probes: Vec::new(),
            next: 0,
            seeding: true,
            last: None,
            labels: Vec::new(),
            words: FlammSampler::Walk {
                probes: Vec::new(),
                curveFeeWad: U256::ZERO,
                bandWad: 0,
                usdIn: U256::ZERO,
                usdOut: U256::ZERO,
                legs: [U256::ZERO; 8],
                bidWad: U256::ZERO,
                truncated: false,
            },
        }
    }

    fn tier(&self, label: U256) -> bool {
        !self.ask.tierA.is_zero() && (label == self.ask.tierA || label == self.ask.tierB)
    }

    /// The walk's next seed, while it seeds and its budget lasts.
    fn seed(&mut self, pool: &FakePool, run: &mut Run<'_>) {
        if !self.seeding {
            return;
        }
        let budget = usize::try_from(self.ask.maxProbes).unwrap_or(usize::MAX);
        let Some(seed) = self
            .ask
            .seeds
            .get(self.next)
            .copied()
            .filter(|_| self.probes.len() < budget)
        else {
            self.seeding = false;
            return;
        };
        let Some((probe, spent)) = run.probe(pool, self.ask, seed) else {
            self.words.truncated = true;
            self.seeding = false;
            return;
        };
        self.next += 1;
        self.seeding = self.goes_on(&probe);
        self.probes.push((probe, spent));
    }

    /// Whether seeding goes on past `probe`: before any priced answer pays at least dust, a
    /// refusal is dust; after one, a refusal, a fall on one label or a third label ends it, as a
    /// clip, a probe short of gas or a malformed answer always does.
    fn goes_on(&mut self, probe: &Probe) -> bool {
        match probe.status {
            ProbeStatus::Clipped | ProbeStatus::Starved | ProbeStatus::Malformed => false,
            ProbeStatus::Refused => self.last.is_none(),
            ProbeStatus::Priced if probe.out < U256::from(DUST_OUT) => true,
            ProbeStatus::Priced => {
                let fell = self
                    .last
                    .is_some_and(|(out, label)| label == probe.fee_wad && probe.out < out);
                if !self.labels.contains(&probe.fee_wad) {
                    self.labels.push(probe.fee_wad);
                }
                self.last = Some((probe.out, probe.fee_wad));
                !fell && self.labels.len() < 3
            }
        }
    }

    /// The request words, where a priced or clipped answer paying at least dust carries a tier
    /// label: the pool's request at the first, the curve's fee where every such answer carries
    /// that one tier label, and on a buy the hook's top bid. A read that fails leaves its words
    /// zero.
    fn read_request(&mut self, chain: &FakeChain, pool: &FakePool, run: &mut Run<'_>) {
        let ask = self.ask;
        if ask.hook.is_zero() {
            return;
        }
        let Some(first) = self
            .probes
            .iter()
            .map(|(probe, _)| *probe)
            .find(|probe| probe.qualifies() && self.tier(probe.fee_wad))
        else {
            return;
        };
        if run.spend(CALL_GAS).is_none() {
            self.words.truncated = true;
            return;
        }
        if pool.fault == Fault::RequestRefused {
            return;
        }
        let Some(words) = pool.request(Some(run.taker), ask.tokenIn, ask.tokenOut, first.ask)
        else {
            return;
        };
        run.seen
            .pair_requests
            .push(RequestSeen {
                from: Some(run.taker),
                token_in: ask.tokenIn,
                amount: first.ask,
                words,
            });
        self.words.bandWad = u64::try_from(words[10]).unwrap_or_default();
        self.words.usdIn = words[11];
        self.words.usdOut = words[12];
        self.words
            .legs
            .copy_from_slice(&words[15..23]);
        let Some(hook) = chain.hooks.get(&ask.hook) else {
            return;
        };
        let mut labels = self
            .probes
            .iter()
            .filter(|(probe, _)| probe.qualifies())
            .map(|(probe, _)| probe.fee_wad);
        let label = labels.next();
        let one_tier =
            labels.all(|other| Some(other) == label) && label.is_some_and(|l| self.tier(l));
        if one_tier {
            if run.spend(CALL_GAS).is_none() {
                self.words.truncated = true;
                return;
            }
            let mut at_zero = words;
            at_zero[13] = U256::ZERO;
            if let Some(fill) = hook.answer(QUOTE_PAIR_SELECTOR, at_zero.len()) {
                run.seen.hook_reads.push(HookReadSeen {
                    hook: ask.hook,
                    selector: QUOTE_PAIR_SELECTOR,
                    from: Some(run.taker),
                    words: at_zero.to_vec(),
                });
                self.words.curveFeeWad = fill[3];
            }
        }
        if words[3] == U256::from(1) {
            if run.spend(CALL_GAS).is_none() {
                self.words.truncated = true;
                return;
            }
            if let Some(state) = hook.answer(LEV_STATE_SELECTOR, words.len()) {
                run.seen.hook_reads.push(HookReadSeen {
                    hook: ask.hook,
                    selector: LEV_STATE_SELECTOR,
                    from: Some(run.taker),
                    words: words.to_vec(),
                });
                self.words.bidWad = state[6];
            }
        }
    }

    /// One probe just above the highest priced answer paying at least dust, where a tier label
    /// priced it, never at or past `2^120` nor at an input already asked.
    fn probe_above(&mut self, pool: &FakePool, run: &mut Run<'_>) {
        if self.ask.hook.is_zero() || self.ask.tierA.is_zero() {
            return;
        }
        let Some(top) = self
            .probes
            .iter()
            .rev()
            .map(|(probe, _)| *probe)
            .find(|probe| probe.status == ProbeStatus::Priced && probe.out >= U256::from(DUST_OUT))
        else {
            return;
        };
        if !self.tier(top.fee_wad) {
            return;
        }
        let above = top.used + (top.used >> ABOVE_SHIFT) + U256::from(1);
        if above.bit_len() > ANSWER_BITS ||
            self.probes
                .iter()
                .any(|(probe, _)| probe.ask == above)
        {
            return;
        }
        let Some(probe) = run.probe(pool, self.ask, above) else {
            self.words.truncated = true;
            return;
        };
        let at = self
            .probes
            .partition_point(|(p, _)| p.ask < above);
        self.probes.insert(at, probe);
    }

    fn finish(self) -> FlammSampler::Walk {
        let probes = self
            .probes
            .into_iter()
            .map(|(probe, spent)| FlammSampler::Probe {
                ask: probe.ask,
                used: probe.used,
                out: probe.out,
                feeWad: probe.fee_wad,
                gasUsed: U256::from(spent),
                status: U256::from(match probe.status {
                    ProbeStatus::Priced => 1u8,
                    ProbeStatus::Clipped => 2,
                    ProbeStatus::Refused => 3,
                    ProbeStatus::Starved => 4,
                    ProbeStatus::Malformed => 5,
                }),
                reason: FixedBytes::ZERO,
            })
            .collect();
        FlammSampler::Walk { probes, ..self.words }
    }
}

/// A block as `eth_getBlockByNumber` returns it, every field alloy requires present.
fn block(timestamp: u64, number: u64) -> Value {
    let zero32 = format!("0x{}", "00".repeat(32));
    json!({
        "hash": zero32,
        "parentHash": zero32,
        "sha3Uncles": zero32,
        "miner": format!("0x{}", "00".repeat(20)),
        "stateRoot": zero32,
        "transactionsRoot": zero32,
        "receiptsRoot": zero32,
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "difficulty": "0x0",
        "number": format!("{number:#x}"),
        "gasLimit": "0x1c9c380",
        "gasUsed": "0x0",
        "timestamp": format!("{timestamp:#x}"),
        "extraData": "0x",
        "mixHash": zero32,
        "nonce": "0x0000000000000000",
        "baseFeePerGas": "0x1",
        "uncles": [],
        "transactions": [],
        "size": "0x0"
    })
}

/// 18-decimal ether at 2,000 USD and 6-decimal USD coin at 1 USD, both priced by the feed.
pub(super) fn eth() -> FakeToken {
    FakeToken {
        address: Address::repeat_byte(0x11),
        decimals: 18,
        usd_wad: Some(U256::from(2_000u64) * U256::from(10u64).pow(U256::from(18))),
        held: U256::ZERO,
    }
}

pub(super) fn usd() -> FakeToken {
    FakeToken {
        address: Address::repeat_byte(0x22),
        decimals: 6,
        usd_wad: Some(U256::from(10u64).pow(U256::from(18))),
        held: U256::ZERO,
    }
}

/// Another 6-decimal USD coin, for a pool's third token.
pub(super) fn tether() -> FakeToken {
    FakeToken { address: Address::repeat_byte(0x33), ..usd() }
}

/// The account behind every venue of the pool at `pool`.
pub(super) fn account_of(pool: Address) -> Address {
    let mut bytes = pool.into_array();
    bytes[0] = 0xac;
    Address::from(bytes)
}

/// An ETH/USD pool at `byte`: 1,998 USD per ETH sold, 2,002 USD per ETH bought, at most 50 ETH or
/// 100,000 USD of input used.
pub(super) fn pool(byte: u8) -> FakePool {
    FakePool {
        address: Address::repeat_byte(byte),
        tokens: vec![eth(), usd()],
        // 1e18 wei -> 1998e6 units; 1e6 units -> 1e18 / 2002 wei
        rates: [(1_998, 1_000_000_000_000), (1_000_000_000_000, 2_002)],
        caps: [
            U256::from(50u64) * U256::from(10u64).pow(U256::from(18)),
            U256::from(100_000_000_000u64),
        ],
        lane_on: false,
        hook: HOOK,
        feed: FEED,
        fault: Fault::None,
        venue_kinds: vec![0],
        venue_status: 0,
        venue_actions: Some(0x3f),
        venue_capabilities: Some(16),
        venue_minimum: None,
        labels: [Vec::new(), Vec::new()],
        band_wad: BAND,
    }
}
