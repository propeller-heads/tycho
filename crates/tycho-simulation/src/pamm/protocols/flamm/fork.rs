//! The feed against a local fork of Base where an Efficient pool's leverage route wins small
//! fills, and against a record of that fork.
//!
//! The fork is Base at block 53,755,892 with a factory, [`FACTORY`], of two pools of cbBTC and
//! USDC over one price feed, one lending on Morpho Blue and one on Aave V3, whose hooks run a build
//! of class [`HOOK_CLASS`] with the row [`ROW_WORDS`]. A live test reads the fork as the feed does,
//! at one block, then swaps each size of a dense grid through the pool from Tycho's router at that
//! block, by `eth_call`. The fork's own methods hand the router both tokens and the pool its
//! allowance, mining no block, as a route hands the pool its input. A quote may pass the swap's
//! output by at most a tolerance: the pool's own rounding and what the noise of its book can move
//! a fill by, from its request words ([`tolerance`]). A quote past it, or a quoted size the swap
//! does not use whole, fails; a size the pool refuses costs a route nothing. Each control settles
//! with one of the rule's tests left out, or a word it reads forced, and must pass the swap by more
//! than the tolerance, or the fork is not at the state its test is for.
//!
//! The live tests need `RPC_URL` to name a local anvil fork at their state, and are ignored by
//! default:
//! - `a_lever_fork_quotes_no_size_past_its_swap`: small sales priced on the leverage route, larger
//!   ones on the curve. With `FLAMM_FORK_EDGE` naming a side, `sale` or `buy`, that side is also
//!   walked with two rungs forced either side of a size the curve prices between two the leverage
//!   route prices, and from 32 seeds from a thousandth of a token to a billion tokens; with
//!   `FLAMM_FORK_RECORD` naming a file, the test writes its record there, as
//!   `testdata/record_base_fork_lever.sh` does.
//! - `a_capped_sale_quotes_no_size_past_its_swap`: the USDC ceiling of `FLAMM_FORK_POOL` set to
//!   each of `FLAMM_FORK_CEILINGS` in turn, from the pool's owner, so that a large sale clips. With
//!   `FLAMM_FORK_KEPT_TOP` set, the hook keeps a leverage top that the pool's preview of a large
//!   sale reads and its swap does not.
//! - `a_buy_at_the_join_quotes_nothing_on_leverage`: the hook's lever-up bid reads zero, and the
//!   leverage route's output on a buy of `FLAMM_FORK_POOL` falls from one size to the next.
//! - `a_token_ceiling_hole_is_not_quoted_over`: a USDC ceiling between two outputs of the leverage
//!   route of `FLAMM_FORK_POOL`, so that `FLAMM_FORK_HOLE` sells on the curve between two sizes
//!   that sell on leverage.
//!
//! - `a_bsc_fork_charges_no_published_fill_over_its_gas`: with `RPC_URL` naming a local fork of BSC
//!   whose factory [`BSC_FACTORY`] holds a pool of BTCB and USDT on each of Aave V3, Venus Core and
//!   Lista Moolah at its seed, the feed serves all three; and at every published input of either
//!   side, after a deposit into the first two, after a borrowing sale and after a buy, which leaves
//!   the Lista Moolah pool's custody holding USDT under its minimum position, the pool's swap of it
//!   from the router pays at least the quote and costs at least the charge.
//!
//! `the_recorded_lever_fork_settles_as_published` needs no node: it settles the walks recorded in
//! `testdata/base_fork_lever.json` again and checks them against the swaps recorded beside them.
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    sync::Arc,
    time::Duration,
};

use alloy::{
    eips::BlockId,
    primitives::{address, b256, keccak256, Address, Bytes as AlloyBytes, B256, U256, U512},
    providers::{Provider, RootProvider},
    rpc::types::{TransactionInput, TransactionRequest},
    sol,
    sol_types::SolCall,
    transports::RpcError,
};
use futures::{stream, StreamExt};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::Instant as TokioInstant;
use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim, Bytes};

use super::{
    feed::{FlammFeed, FlammFeedBuilder},
    gas::tests::table_serving,
    gas_limit_addition, hook_class,
    ladder::{
        settle, settle_with, AboveTest, Ladder, Probe, ProbeStatus, Request, RoomTest, Row, Rule,
        Segment, Settled, Side, Terms, Trust, ABOVE_SHIFT, DUST_OUT,
    },
    reader::{
        hooksCall, pairRequestCall, paramsCall, previewSwapCall, tokenConfigCall, BookRead,
        Catalogue, DirectionRead, HookShape, PoolInfo, PoolRead, LEV_STATE_SELECTOR,
    },
    router,
    sampler_evm::sweep,
    state::FlammPairState,
    POOL_ATTRIBUTE,
};
use crate::{
    book::{test_token_map, BookFeedConfig, Books},
    evm::protocol::u256_num::biguint_to_u256,
    snapshot_feed::http::{default_http_feed_config, HttpFeedConfig},
};

sol! {
    function swap(
        address tokenIn, address tokenOut, uint256 amountIn, uint256 minAmountOut, address to,
        uint256 deadline
    ) external returns (uint256 amountInUsed, uint256 amountOut);
    function core() external view returns (address);
    function owner() external view returns (address);
    function setTokenConfig(
        uint8 t, uint64 swapPriceBandWad, uint64 feeFloorWad, uint256 maxSwapNotional,
        uint256 reserveTarget
    ) external;
    function balanceOf(address owner) external view returns (uint256);
    function depositProRata(
        uint256 poolAssets, address receiver, uint256[] maxLoanAssetIn,
        uint256[] minLoanAssetOut, uint256 minSharesOut, uint256 deadline
    ) external returns (uint256 shares, uint256 loanAssetIn, uint256 loanAssetOut);
}

/// The fork's factory, price feed, pools and tokens.
const FACTORY: Address = address!("5fef6c2d08257228652134020f459f9c8c6e67bd");
const PRICE_FEED: Address = address!("f63fbd0223d7c3a1641bf4539a3dd49c632b1b84");
const MORPHO_POOL: Address = address!("249134586c7ce04d0a6c3a50ef6e7468bba75a18");
const AAVE_POOL: Address = address!("9dac28b1b337d48c70bc2d873557af7f47fc2d95");
const POOLS: [Address; 2] = [MORPHO_POOL, AAVE_POOL];
const CBBTC: Address = address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf");
const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
/// The class of the hooks' build on the fork, which the feed lists as an Efficient pools hook.
const HOOK_CLASS: B256 = b256!("5b91b2be1b1b06dadba76c992e5c6a82ef532ffeda4fe55de06e2fe6b99c42bd");
/// Words 27-34 of the hooks' `params()`: spreads of 1.75 % and 1.3 %, caps of .5 % and 2 %, a
/// ceiling of 2.2, a band of 5 %, no feed gate and a floor of 1.7.
const ROW_WORDS: [u64; 8] = [
    17_500_000_000_000_000,
    13_000_000_000_000_000,
    5_000_000_000_000_000,
    20_000_000_000_000_000,
    2_200_000_000_000_000_000,
    50_000_000_000_000_000,
    0,
    1_700_000_000_000_000_000,
];
/// Where the row's words start in `params()`.
const ROW_AT: usize = 27;
const HAIRCUT_BPS: u64 = 2;
/// Where every swap pays out.
const RECEIVER: Address = address!("000000000000000000000000000000000000dEaD");
/// The BSC fork's factory and price feed, its pools of BTCB and USDT on Aave V3, Venus Core and
/// Lista Moolah, each at its seed, and a liquidity provider its allowlists admit, who holds BTCB
/// and lets the Aave V3 and Venus Core pools pull it.
const BSC_FACTORY: Address = address!("Fad5ce897B786e9b0168a8C93E1918f297521A1A");
const BSC_PRICE_FEED: Address = address!("08De366BDf9E584AC201433969544C036596a6ea");
const BSC_AAVE_POOL: Address = address!("3e91165c0219d41fC2F9a2B60b81f85D833B6748");
const BSC_VENUS_POOL: Address = address!("A7721E9862F886D23FD1009DA35DC7Aae9EEc145");
const BSC_LISTA_POOL: Address = address!("ff0A9616DA39e7cDc6D92CF350E6364298ECD45C");
const BSC_SERVED: [Address; 3] = [BSC_AAVE_POOL, BSC_VENUS_POOL, BSC_LISTA_POOL];
const BSC_DEPOSITED: [Address; 2] = [BSC_AAVE_POOL, BSC_VENUS_POOL];
const BSC_LP: Address = address!("e12971ad21967a574f16a4bf33bf90dc69f50eea");
const BTCB: Address = address!("7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c");
const USDT: Address = address!("55d398326f99059fF775485246999027B3197955");
/// What the liquidity provider adds to the Aave V3 and Venus Core pools first: 1.3 BTCB, about
/// 100,000 USD.
const BSC_DEPOSIT: u64 = 1_300_000_000_000_000_000;
/// How much less the pool's own frame costs inside Tycho's route on BSC than the same fill sent to
/// the pool straight, paying a receiver that holds the output: measured through the chain's router
/// with a `PropAMMExecutor` on a fork of this deployment, the same on every pool and state.
const BSC_ROUTE_SAVING: u64 = 4_500;
/// A sale large enough to clip at every ceiling the capped test sets: 200 cbBTC.
const LARGE_SALE: u64 = 20_000_000_000;
/// The record of the lever fork, which `testdata/record_base_fork_lever.sh` writes.
const RECORD: &str = include_str!("testdata/base_fork_lever.json");
/// Sizes evenly in log across a ladder, and across each flat span, on the live grid and on the
/// record's.
const GRID: usize = 2_000;
const ACROSS_FLAT: u64 = 300;
const RECORD_GRID: usize = 80;
const RECORD_ACROSS_FLAT: u64 = 40;
/// Sizes either side of the hole the record keeps.
const RECORD_AROUND_HOLE: u64 = 40;
/// The coarse scans' sizes, how close a bisection takes a change of route before every size is
/// previewed, and how far either side of it.
const SCAN: usize = 400;
const BISECTED: u64 = 400;
const DENSE: u64 = 2_500;
/// Swaps and previews in flight at once.
const IN_FLIGHT: usize = 16;
const WAD: u64 = 1_000_000_000_000_000_000;

type Pair = (Address, Address);
/// A pool's swaps by size: the input it used and its output, `None` where it reverts.
type Swaps = BTreeMap<U256, Option<(U256, U256)>>;
/// A pool's previews by size: input used, output and label, `None` where it reverts.
type Previews = BTreeMap<U256, Option<(U256, U256, U256)>>;

fn ten(exponent: u8) -> U512 {
    U512::from(10u64).pow(U512::from(exponent))
}

fn wide(value: U256) -> U512 {
    U512::from(value)
}

fn narrow(value: U512) -> U256 {
    U256::checked_from_limbs_slice(value.as_limbs()).expect("an amount under 2^256")
}

/// `value` as a float: every amount on the fork fits in 128 bits.
fn float(value: U512) -> f64 {
    u128::try_from(value).map_or(f64::MAX, |v| v as f64)
}

fn signed(value: U512) -> i128 {
    u128::try_from(value)
        .ok()
        .and_then(|v| i128::try_from(v).ok())
        .expect("an amount on the fork fits in 127 bits")
}

/// A float rounded down to a whole number.
fn floor(value: f64) -> U256 {
    U256::try_from(value.max(0.0).floor()).expect("a size under 2^256")
}

/// A local fork, read and swapped on from Tycho's router.
struct Fork {
    url: String,
    provider: RootProvider,
    taker: Address,
}

impl Fork {
    /// The node `RPC_URL` names, which must be local: the tests hand the router tokens there.
    fn connect() -> Self {
        Self::connect_to(Chain::Base)
    }

    /// The node `RPC_URL` names, a local fork of `chain`, read and swapped on from its router.
    fn connect_to(chain: Chain) -> Self {
        let url =
            env::var("RPC_URL").unwrap_or_else(|_| panic!("RPC_URL names a local fork of {chain}"));
        assert!(
            url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"),
            "a local fork only, as the tests hand the router tokens there: {url}"
        );
        let provider = RootProvider::new_http(url.parse().expect("RPC_URL is a URL"));
        Self { url, provider, taker: router(chain).expect("the chain has a router") }
    }

    async fn raw(&self, method: &'static str, params: Value) -> Value {
        self.provider
            .raw_request::<_, Value>(method.into(), params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    async fn head(&self) -> u64 {
        self.provider
            .get_block_number()
            .await
            .expect("the fork's head")
    }

    /// The router holds both of `pool`'s tokens and the pool may pull them, as in a route. The
    /// fork's own methods set both and mine no block.
    async fn fund(&self, pool: Address) {
        for token in [CBBTC, USDC] {
            self.raw("anvil_dealERC20", json!([self.taker, token, U256::from(1u128 << 100)]))
                .await;
            self.raw("anvil_setERC20Allowance", json!([self.taker, pool, token, U256::MAX]))
                .await;
        }
    }

    /// `data` called on `to` from the router at `block`; `None` where it reverts.
    async fn call(&self, to: Address, data: Vec<u8>, block: u64) -> Option<AlloyBytes> {
        let tx = TransactionRequest::default()
            .from(self.taker)
            .to(to)
            .gas_limit(30_000_000)
            .input(TransactionInput::new(data.into()));
        match self
            .provider
            .call(tx)
            .block(BlockId::number(block))
            .await
        {
            Ok(answer) => Some(answer),
            Err(RpcError::ErrorResp(payload))
                if payload.code == 3 || payload.as_revert_data().is_some() =>
            {
                None
            }
            Err(error) => panic!("a call to {to} at block {block}: {error}"),
        }
    }

    async fn preview(
        &self,
        pool: Address,
        (token_in, token_out): Pair,
        amount: U256,
        block: u64,
    ) -> Option<(U256, U256, U256)> {
        let data = previewSwapCall { tokenIn: token_in, tokenOut: token_out, amountIn: amount };
        let answer = self
            .call(pool, data.abi_encode(), block)
            .await?;
        let quote = previewSwapCall::abi_decode_returns(&answer).expect("a preview's three words");
        Some((quote.amountInUsed, quote.amountOut, quote.feeWad))
    }

    /// The pool's swap of `amount` from the router at `block`, its output to [`RECEIVER`].
    async fn swap(
        &self,
        pool: Address,
        (token_in, token_out): Pair,
        amount: U256,
        block: u64,
    ) -> Option<(U256, U256)> {
        let data = swapCall {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount,
            minAmountOut: U256::ZERO,
            to: RECEIVER,
            deadline: U256::MAX >> 1,
        };
        let answer = self
            .call(pool, data.abi_encode(), block)
            .await?;
        let fill = swapCall::abi_decode_returns(&answer).expect("a swap's two words");
        Some((fill.amountInUsed, fill.amountOut))
    }

    /// The pool's swaps of the sizes in `sizes` that `swaps` lacks, added to it.
    async fn swaps(
        &self,
        pool: Address,
        pair: Pair,
        sizes: &BTreeSet<U256>,
        block: u64,
        swaps: &mut Swaps,
    ) {
        let missing: Vec<U256> = sizes
            .iter()
            .filter(|size| !swaps.contains_key(size))
            .copied()
            .collect();
        let got: Vec<(U256, Option<(U256, U256)>)> = stream::iter(missing)
            .map(|size| async move { (size, self.swap(pool, pair, size, block).await) })
            .buffer_unordered(IN_FLIGHT)
            .collect()
            .await;
        swaps.extend(got);
    }

    async fn previews(
        &self,
        pool: Address,
        pair: Pair,
        sizes: impl IntoIterator<Item = U256>,
        block: u64,
    ) -> Previews {
        stream::iter(sizes)
            .map(|size| async move {
                (
                    size,
                    self.preview(pool, pair, size, block)
                        .await,
                )
            })
            .buffer_unordered(IN_FLIGHT)
            .collect()
            .await
    }

    /// A transaction from `from`, impersonated, in a block one second after the head; it must
    /// succeed.
    async fn send(&self, from: Address, to: Address, data: Vec<u8>) {
        let receipt = self.transact(from, to, data).await;
        assert_eq!(receipt["status"], "0x1", "a transaction from {from} to {to} failed");
    }

    /// A transaction from `from`, impersonated, in a block one second after the head: its
    /// receipt.
    async fn transact(&self, from: Address, to: Address, data: Vec<u8>) -> Value {
        let head = self
            .raw("eth_getBlockByNumber", json!(["latest", false]))
            .await;
        let at = head["timestamp"]
            .as_str()
            .and_then(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok())
            .expect("the head's timestamp");
        self.raw("evm_setNextBlockTimestamp", json!([at + 1]))
            .await;
        self.raw("anvil_impersonateAccount", json!([from]))
            .await;
        self.raw("anvil_setBalance", json!([from, U256::from(10u64).pow(U256::from(22))]))
            .await;
        let transaction = json!({
            "from": from, "to": to, "data": AlloyBytes::from(data), "gas": "0x1c9c380",
        });
        let hash = self
            .raw("eth_sendTransaction", json!([transaction]))
            .await;
        let mut receipt = Value::Null;
        for _ in 0..1_200 {
            receipt = self
                .raw("eth_getTransactionReceipt", json!([hash]))
                .await;
            if !receipt.is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!receipt.is_null(), "no receipt for a transaction from {from} to {to}");
        self.raw("anvil_stopImpersonatingAccount", json!([from]))
            .await;
        receipt
    }

    async fn balance(&self, token: Address, owner: Address, block: u64) -> U256 {
        let answer = self
            .call(token, balanceOfCall { owner }.abi_encode(), block)
            .await
            .expect("a balance answers");
        balanceOfCall::abi_decode_returns(&answer).expect("a balance's word")
    }

    /// The class of the code of the hook that prices `pool`'s hook pair.
    async fn hook_class_of(&self, pool: Address, block: u64) -> B256 {
        let answer = self
            .call(pool, hooksCall {}.abi_encode(), block)
            .await
            .expect("the pool's hooks");
        let hook = hooksCall::abi_decode_returns(&answer)
            .expect("a hook set")
            .hook;
        let code = self
            .provider
            .get_code_at(hook)
            .await
            .expect("the hook's code");
        hook_class(&code)
    }

    /// `pool`'s swap of `amount` from the router at the head, its input found: the fork's own
    /// method first gives the pool `amount` more of the input, as a route's transfer does. Its
    /// output to [`RECEIVER`] and its gas net of the refund, the receipt's less the transaction's
    /// intrinsic gas; `None` where it reverts. With `keep` false the fork goes back to where it
    /// was.
    async fn fill_found(
        &self,
        pool: Address,
        (token_in, token_out): Pair,
        amount: U256,
        keep: bool,
    ) -> Option<(U256, u64)> {
        let snapshot = if keep {
            None
        } else {
            Some(
                self.raw("evm_snapshot", json!([]))
                    .await,
            )
        };
        let block = self.head().await;
        let held = self
            .balance(token_in, pool, block)
            .await;
        self.raw("anvil_dealERC20", json!([pool, token_in, held + amount]))
            .await;
        let data = swapCall {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount,
            minAmountOut: U256::ZERO,
            to: RECEIVER,
            deadline: U256::MAX >> 1,
        }
        .abi_encode();
        let receipt = self
            .transact(self.taker, pool, data.clone())
            .await;
        if let Some(snapshot) = snapshot {
            let back = self
                .raw("evm_revert", json!([snapshot]))
                .await;
            assert_eq!(back, Value::Bool(true), "the fork goes back to its snapshot");
        }
        if receipt["status"] != "0x1" {
            return None;
        }
        let used = quantity(&receipt["gasUsed"]);
        Some((transferred(&receipt, token_out, RECEIVER), used - intrinsic_gas(&data)))
    }

    /// Every slot `pool`'s swap of each of `sizes` reads, fetched into the fork before any
    /// snapshot, as a fork that keeps no storage fetches again what it read after one: each swap
    /// asked by `eth_call`, its input pulled from the router, which the fork's methods fund.
    async fn warm(&self, pool: Address, pair: Pair, sizes: &BTreeSet<U256>) {
        for token in [pair.0, pair.1] {
            self.raw("anvil_dealERC20", json!([self.taker, token, U256::from(1u128 << 100)]))
                .await;
            self.raw("anvil_setERC20Allowance", json!([self.taker, pool, token, U256::MAX]))
                .await;
        }
        let block = self.head().await;
        let mut swaps = Swaps::new();
        self.swaps(pool, pair, sizes, block, &mut swaps)
            .await;
    }

    async fn address_of(&self, to: Address, data: Vec<u8>, block: u64) -> Address {
        let answer = self
            .call(to, data, block)
            .await
            .expect("an address view answers");
        Address::from_slice(&answer[12..32])
    }

    async fn token_config(
        &self,
        pool: Address,
        block: u64,
    ) -> <tokenConfigCall as SolCall>::Return {
        let config = self
            .call(pool, tokenConfigCall { t: 1 }.abi_encode(), block)
            .await
            .expect("token 1's config");
        tokenConfigCall::abi_decode_returns(&config).expect("a token config")
    }

    /// Token 1's ceiling set to `ceiling` by the pool's own `setTokenConfig` from its core's
    /// owner, its other words kept. A lower ceiling is a tightening, so it applies at once.
    async fn set_ceiling(&self, pool: Address, ceiling: U256) {
        let block = self.head().await;
        let config = self.token_config(pool, block).await;
        let core = self
            .address_of(pool, coreCall {}.abi_encode(), block)
            .await;
        let owner = self
            .address_of(core, ownerCall {}.abi_encode(), block)
            .await;
        let set = setTokenConfigCall {
            t: 1,
            swapPriceBandWad: config.swapPriceBandWad,
            feeFloorWad: config.feeFloorWad,
            maxSwapNotional: ceiling,
            reserveTarget: config.reserveTarget,
        };
        self.send(owner, pool, set.abi_encode())
            .await;
        let set = self
            .token_config(pool, self.head().await)
            .await;
        assert_eq!(set.maxSwapNotional, ceiling, "the ceiling set");
    }
}

/// The feed over the fork's factory, its hooks' class and its price feed, cbBTC and USDC its
/// tokens, the node's block taken as it is. The hooks' build is older than the measured table's,
/// so a table that serves their class charges its swaps: these tests check quotes, not gas.
fn feed(url: &str) -> FlammFeed {
    let (btc, usdc) = (Bytes::from(CBBTC.to_vec()), Bytes::from(USDC.to_vec()));
    let config = BookFeedConfig {
        chain: Chain::Base,
        tokens: Arc::new(test_token_map(&[(&btc, "cbBTC", 8), (&usdc, "USDC", 6)])),
        min_tvl_usd: 0.0,
    };
    FlammFeedBuilder::new(config, url.to_string())
        .factories(vec![Bytes::from(FACTORY.to_vec())])
        .hook_classes(vec![(HOOK_CLASS, HookShape::Everlong)])
        .price_feeds(vec![Bytes::from(PRICE_FEED.to_vec())])
        .gas_table(table_serving(HOOK_CLASS))
        .haircut_bps(HAIRCUT_BPS)
        .max_block_lag(None)
        .feed_config(HttpFeedConfig {
            request_timeout: Duration::from_secs(120),
            ..default_http_feed_config()
        })
        .build()
        .expect("the feed builds")
}

/// One round of discovery and one read at `block`: the pools' walks, and the books the feed
/// publishes of them.
async fn poll(feed: &FlammFeed, block: u64) -> (BookRead, Books) {
    feed.reader()
        .discover(&mut Catalogue::default())
        .await;
    let read = feed
        .reader()
        .read(block, TokioInstant::now() + Duration::from_secs(600))
        .await
        .expect("the feed reads the fork");
    assert_eq!(read.block, block);
    let books = feed.books(&read);
    (read, books)
}

fn pool_read(read: &BookRead, pool: Address) -> &PoolRead {
    read.pools
        .iter()
        .find(|p| p.info.pool == pool)
        .unwrap_or_else(|| panic!("the feed reads {pool}, whose hook must be of {HOOK_CLASS}"))
}

/// The pool's book as the feed publishes it.
fn published(books: &Books, pool: Address) -> &FlammPairState {
    book_of(books, pool).unwrap_or_else(|| panic!("a book of {pool}"))
}

/// The pool's book, where the feed publishes one.
fn book_of(books: &Books, pool: Address) -> Option<&FlammPairState> {
    let pool = Bytes::from(pool.to_vec());
    books
        .values()
        .find(|book| {
            book.component
                .static_attributes
                .get(POOL_ATTRIBUTE) ==
                Some(&pool)
        })
        .and_then(|book| {
            book.state
                .as_any()
                .downcast_ref::<FlammPairState>()
        })
}

fn side_of(walk: &DirectionRead) -> Side {
    if walk.plan.token_in == 0 {
        Side::Sale
    } else {
        Side::Buy
    }
}

fn pair_of(info: &PoolInfo, walk: &DirectionRead) -> Pair {
    (info.tokens[walk.plan.token_in].address, info.tokens[walk.plan.token_out].address)
}

/// The walk of `side`.
fn walk_of(pool: &PoolRead, side: Side) -> &DirectionRead {
    pool.directions
        .iter()
        .find(|walk| side_of(walk) == side)
        .unwrap_or_else(|| panic!("{} reads a {side:?}", pool.info.pool))
}

/// What a direction's tolerance and checks read besides its walk: the pool's request from the
/// router, the hook's `levState` of that request, and the hook's `params()`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Words {
    request: Vec<U256>,
    lev: Vec<U256>,
    params: Vec<U256>,
}

async fn words(fork: &Fork, info: &PoolInfo, pair: Pair, at: U256, block: u64) -> Words {
    let request = pairRequestCall { tokenIn: pair.0, tokenOut: pair.1, amountIn: at };
    let request = fork
        .call(info.pool, request.abi_encode(), block)
        .await
        .expect("the pool's request");
    let request = pairRequestCall::abi_decode_returns(&request).expect("26 words");
    let data: Vec<u8> = LEV_STATE_SELECTOR
        .iter()
        .copied()
        .chain(
            request
                .iter()
                .flat_map(|word| word.to_be_bytes::<32>()),
        )
        .collect();
    let lev = fork
        .call(info.hook, data, block)
        .await
        .expect("the hook's levState");
    let lev: Vec<U256> = lev
        .as_chunks::<32>()
        .0
        .iter()
        .take(8)
        .map(|word| U256::from_be_slice(word))
        .collect();
    assert_eq!(lev.len(), 8, "levState's eight words");
    let params = fork
        .call(info.hook, paramsCall {}.abi_encode(), block)
        .await
        .expect("the hook's params");
    let params = paramsCall::abi_decode_returns(&params).expect("35 words");
    Words { request: request.to_vec(), lev, params: params.to_vec() }
}

/// The most a quote may pass the pool's fill, in output units: the pool's own rounding on a sale,
/// and on either side what the noise of its book can move a fill by, read from the request words,
/// `levState`'s book value (word 0) and the hook's amplification and spans (`params()` words 0-2).
/// 4 where the request reads no price.
fn tolerance(side: Side, decimals_in: u8, decimals_out: u8, words: &Words) -> U256 {
    let request = &words.request;
    let (band, usd_in, usd_out) = (request[10], request[11], request[12]);
    let legs: Vec<U512> = request[15..23]
        .iter()
        .copied()
        .map(wide)
        .collect();
    let (held_in, supplied_in, posted_in, owed_in) = (legs[0], legs[1], legs[2], legs[3]);
    let (held_out, supplied_out, posted_out, owed_out) = (legs[4], legs[5], legs[6], legs[7]);
    let sale = side == Side::Sale;
    let (usd_0, usd_1, decimals_0, decimals_1) = if sale {
        (usd_in, usd_out, decimals_in, decimals_out)
    } else {
        (usd_out, usd_in, decimals_out, decimals_in)
    };
    let e36 = ten(36);
    let p36 = if usd_1.is_zero() {
        U512::ZERO
    } else {
        wide(usd_0) * ten(decimals_1) * e36 / (wide(usd_1) * ten(decimals_0))
    };
    let value_out = held_out + supplied_out + posted_out;
    let unread = band.is_zero() ||
        usd_in.is_zero() ||
        usd_out.is_zero() ||
        p36.is_zero() ||
        (!sale && value_out.is_zero());
    if unread && (sale || !value_out.is_zero() || p36.is_zero()) {
        return U256::from(4);
    }
    let amplification = float(wide(words.params[0])) / 1e18;
    let span = float(wide(words.params[1].min(words.params[2]))) / 1e18;
    let book = 1.0 + 1.0 / (amplification * (span - 1.0));
    let root = (float(wide(usd_0) * ten(18 - decimals_0)) / float(wide(usd_1))).sqrt();
    let cv_wad = wide(words.lev[0]);
    if !sale {
        let idle_in = (held_in + supplied_in).saturating_sub(owed_in) * e36 / p36;
        let cv_out = cv_wad / ten(18 - decimals_in) * e36 / p36;
        let noise = 8e-18 * float(value_out + idle_in + cv_out) * book * (1.0 + 1.0 / root);
        return U256::from(4) + floor(noise);
    }
    let unit = p36 / e36 + U512::from(1);
    let cv = if cv_wad.is_zero() {
        U512::from(2) *
            ((held_in + supplied_in + posted_in) * p36 / e36 +
                (held_out + supplied_out).saturating_sub(owed_out))
    } else {
        cv_wad / ten(18 - decimals_out)
    };
    let noise = (4e-18 + 2.5e-18 * root) * float(cv) * book;
    U256::from(4) + narrow(U512::from(2) * unit) + floor(noise)
}

/// The room an answer of `used` for `out` has over the band's lower edge and under its upper
/// one, and the margin the rule asks of both, worked out here from the request words; `None`
/// where the request reads no price.
fn room_of(terms: &Terms, used: U256, out: U256) -> Option<(i128, i128, i128)> {
    let request = &terms.request;
    let sale = terms.side == Side::Sale;
    let (usd_0, usd_1, decimals_0, decimals_1) = if sale {
        (request.usd_in, request.usd_out, terms.decimals_in, terms.decimals_out)
    } else {
        (request.usd_out, request.usd_in, terms.decimals_out, terms.decimals_in)
    };
    if usd_0.is_zero() || usd_1.is_zero() || request.band_wad.is_zero() {
        return None;
    }
    let e36 = ten(36);
    let p36 = wide(usd_0) * ten(decimals_1) * e36 / (wide(usd_1) * ten(decimals_0));
    let legs: Vec<U512> = request
        .legs
        .iter()
        .copied()
        .map(wide)
        .collect();
    let value_out = legs[4] + legs[5] + legs[6];
    if p36.is_zero() || (!sale && value_out.is_zero()) {
        return None;
    }
    let (wad, band) = (U512::from(WAD), wide(terms.row_band.min(request.band_wad)));
    let unit = if sale { p36 / e36 + U512::from(1) } else { U512::from(1) };
    let need = U512::from(2) + U512::from(3) * unit;
    let (used, out) = (wide(used), wide(out));
    let (low, high, margin) = if sale {
        let scale = e36 * wad;
        (
            used * p36 * (wad - band) / scale + U512::from(1),
            used * p36 * (wad + band) / scale,
            need + used / ten(36 - terms.decimals_out) + U512::from(1),
        )
    } else {
        let idle_in = (legs[0] + legs[1]).saturating_sub(legs[3]) * e36 / p36;
        let scale = p36 * wad;
        (
            used * e36 * (wad - band) / scale + U512::from(1),
            used * e36 * (wad + band) / scale,
            need + (value_out + idle_in) / ten(16) +
                out * ten(terms.decimals_in) / p36 +
                U512::from(1),
        )
    };
    Some((signed(out) - signed(low), signed(high) - signed(out), signed(margin)))
}

/// The probe a ladder's rung at `input` stands on: the first live probe that used it.
fn probe_at(probes: &[Probe], input: U256) -> Probe {
    *probes
        .iter()
        .find(|p| matches!(p.status, ProbeStatus::Priced | ProbeStatus::Clipped) && p.used == input)
        .expect("each rung stands on a probe that used its input")
}

/// The inputs of the priced points of the rule's first run, worked out here: `probes` less dust
/// and less the leverage answers short of the room test's margin, taken in order up to the first
/// answer that starves or is malformed, refuses past a point paying at least [`DUST_OUT`], uses no
/// more than the last point, falls other than off a leverage label or carries a third label, or
/// up to a clip. A refusal before any point, or past one paying less than [`DUST_OUT`], clears
/// them. Only these back a leverage point: a priced answer past where they stop backs nothing.
fn backers(terms: &Terms, probes: &[Probe]) -> BTreeSet<U256> {
    let live = |p: &Probe| matches!(p.status, ProbeStatus::Priced | ProbeStatus::Clipped);
    let dust = U256::from(DUST_OUT);
    let dusty = probes
        .iter()
        .any(|p| live(p) && p.out >= dust);
    let roomy = |p: &Probe| {
        !terms.tiers.contains(p.fee_wad) ||
            room_of(terms, p.used, p.out)
                .is_none_or(|(under, over, margin)| under >= margin && over >= margin)
    };
    let mut points: Vec<Probe> = Vec::new();
    for p in probes
        .iter()
        .filter(|p| !live(p) || ((!dusty || p.out >= dust) && roomy(p)))
    {
        let last = points.last().copied();
        if p.status == ProbeStatus::Refused && last.is_none_or(|l| l.out < dust) {
            points.clear();
            continue;
        }
        let labels: BTreeSet<U256> = points
            .iter()
            .map(|q| q.fee_wad)
            .collect();
        let ends = !live(p) ||
            last.is_some_and(|l| {
                let falls =
                    p.out < l.out && !(terms.tiers.contains(l.fee_wad) && p.fee_wad != l.fee_wad);
                p.used <= l.used || falls || (labels.len() >= 2 && !labels.contains(&p.fee_wad))
            });
        if ends {
            break;
        }
        points.push(*p);
        if p.status == ProbeStatus::Clipped {
            break;
        }
    }
    points
        .iter()
        .filter(|p| p.status == ProbeStatus::Priced)
        .map(|p| p.used)
        .collect()
}

/// The leverage-labelled points of `ladder`, each of which must have the room test's margin both
/// ways and, at least `used >> ABOVE_SHIFT` above it, a priced point of the rule's first run
/// ([`backers`]). Their count, and the least room any of them has past its margin.
fn tier_points(terms: &Terms, probes: &[Probe], ladder: &Ladder, what: &str) -> (usize, i128) {
    let backers = backers(terms, probes);
    let (mut count, mut slack) = (0, i128::MAX);
    for rung in ladder.rungs() {
        let probe = probe_at(probes, rung.input);
        if !terms.tiers.contains(probe.fee_wad) {
            continue;
        }
        count += 1;
        let (under, over, margin) = room_of(terms, probe.used, probe.out)
            .unwrap_or_else(|| panic!("{what}: a leverage point where the request reads no price"));
        assert!(
            under >= margin && over >= margin,
            "{what}: the leverage point at {} has {under} and {over} units of room, under its \
             margin of {margin}",
            probe.used
        );
        slack = slack.min(under.min(over) - margin);
        let backing = probe.used + (probe.used >> ABOVE_SHIFT);
        assert!(
            backers
                .range(backing..)
                .next()
                .is_some(),
            "{what}: no priced point of the first run at or past {backing}, above the leverage \
             point at {}",
            probe.used
        );
    }
    (count, slack)
}

/// The labels of `ladder`'s points, and how many of its spans are flat.
fn labels(probes: &[Probe], ladder: &Ladder) -> (BTreeSet<U256>, usize) {
    let rungs = ladder.rungs();
    let labels = rungs
        .iter()
        .map(|rung| probe_at(probes, rung.input).fee_wad)
        .collect();
    let flats = rungs
        .iter()
        .filter(|rung| rung.segment == Segment::Flat)
        .count();
    (labels, flats)
}

/// How many of `ladder`'s points stand on a leverage-labelled probe.
fn leverage_points(terms: &Terms, probes: &[Probe], ladder: &Ladder) -> usize {
    ladder
        .rungs()
        .iter()
        .filter(|rung| {
            terms
                .tiers
                .contains(probe_at(probes, rung.input).fee_wad)
        })
        .count()
}

/// `count` sizes evenly in log over `[low, high]`, rounded down.
fn log_sizes(low: U256, high: U256, count: usize) -> BTreeSet<U256> {
    let (lo, hi) = (float(wide(low)), float(wide(high)));
    let steps = (count - 1) as f64;
    (0..count)
        .map(|k| floor(lo * (hi / lo).powf(k as f64 / steps)).clamp(low, high))
        .collect()
}

/// Sizes evenly in log across `ladders`, each rung's input and one unit either side of it, and
/// `across_flat` sizes across each flat span.
fn grid(ladders: &[&Ladder], count: usize, across_flat: u64) -> BTreeSet<U256> {
    let low = ladders
        .iter()
        .filter_map(|ladder| ladder.first())
        .map(|rung| rung.input)
        .min();
    let high = ladders
        .iter()
        .filter_map(|ladder| ladder.last())
        .map(|rung| rung.input)
        .max();
    let (Some(low), Some(high)) = (low, high) else { return BTreeSet::new() };
    let mut grid = log_sizes(low, high, count);
    let one = U256::from(1);
    for ladder in ladders {
        let rungs = ladder.rungs();
        for rung in rungs {
            grid.extend([rung.input - one, rung.input, rung.input + one]);
        }
        for pair in rungs.windows(2) {
            if pair[1].segment == Segment::Flat {
                let (below, span) = (pair[0].input, pair[1].input - pair[0].input);
                grid.extend(
                    (0..across_flat)
                        .map(|k| below + span * U256::from(k) / U256::from(across_flat - 1)),
                );
            }
        }
    }
    grid
}

/// A quote's sizes against the pool's swaps.
#[derive(Clone, Copy, Debug, Serialize)]
struct Tally {
    quoted: usize,
    unquoted: usize,
    /// Quoted where the pool refuses the swap, which costs a route nothing.
    refused: usize,
    /// Quoted where the swap uses less than the whole input.
    clipped: usize,
    /// The most a quote passes the swap's output, in units, negative where every quote is under,
    /// with where and what the swap paid there; `None` where no quoted size was swapped.
    worst: Option<i128>,
    at: U256,
    paid: U256,
}

impl Tally {
    fn of(quote: impl Fn(U256) -> Option<U256>, swaps: &Swaps, sizes: &BTreeSet<U256>) -> Self {
        let mut tally = Self {
            quoted: 0,
            unquoted: 0,
            refused: 0,
            clipped: 0,
            worst: None,
            at: U256::ZERO,
            paid: U256::ZERO,
        };
        for size in sizes {
            let Some(quoted) = quote(*size) else {
                tally.unquoted += 1;
                continue;
            };
            tally.quoted += 1;
            let Some((used, paid)) = swaps.get(size).copied().flatten() else {
                tally.refused += 1;
                continue;
            };
            if used != *size {
                tally.clipped += 1;
            }
            let over = signed(wide(quoted)) - signed(wide(paid));
            if tally
                .worst
                .is_none_or(|worst| over > worst)
            {
                (tally.worst, tally.at, tally.paid) = (Some(over), *size, paid);
            }
        }
        tally
    }

    fn bp(&self) -> Option<f64> {
        self.worst
            .map(|worst| worst as f64 / float(wide(self.paid)).max(1.0) * 1e4)
    }

    /// No quote past the swap by more than `tolerance`, and no quoted size the swap clips.
    fn holds(&self, what: &str, tolerance: U256) {
        assert_eq!(self.clipped, 0, "{what}: a quoted size the swap does not use whole: {self:?}");
        assert!(
            self.worst
                .is_none_or(|worst| worst <= signed(wide(tolerance))),
            "{what}: {self:?} passes the swap by {:?} bp, past a tolerance of {tolerance}",
            self.bp()
        );
    }

    /// Some quote past the swap by more than `tolerance`: the control shows its state.
    fn passes(&self, what: &str, tolerance: U256) {
        assert!(
            self.worst
                .is_some_and(|worst| worst > signed(wide(tolerance))),
            "{what}: the control does not pass the swap by more than {tolerance}, {self:?}: the \
             fork is not at its state"
        );
    }
}

/// The book's own quote, net of the haircut, selling `pair.0` for `pair.1`.
fn book_quote(state: &FlammPairState, pair: Pair) -> impl Fn(U256) -> Option<U256> + '_ {
    let (token_in, token_out) = if state.base_token.address == *pair.0.as_slice() {
        (&state.base_token, &state.quote_token)
    } else {
        (&state.quote_token, &state.base_token)
    };
    move |size: U256| {
        let amount = BigUint::from_bytes_be(&size.to_be_bytes::<32>());
        state
            .get_amount_out(amount, token_in, token_out)
            .ok()
            .map(|result| biguint_to_u256(&result.amount))
    }
}

/// The book's ladder selling `pair.0` for `pair.1`.
fn book_ladder(state: &FlammPairState, pair: Pair) -> &Ladder {
    if state.base_token.address == *pair.0.as_slice() {
        &state.sell_base
    } else {
        &state.sell_quote
    }
}

/// `probes` less the probe above: the one asked at `used + (used >> ABOVE_SHIFT) + 1` of a
/// leverage-labelled priced probe paying at least [`DUST_OUT`].
fn less_above(probes: &[Probe], terms: &Terms) -> Vec<Probe> {
    let above: BTreeSet<U256> = probes
        .iter()
        .filter(|p| p.status == ProbeStatus::Priced && p.out >= U256::from(DUST_OUT))
        .filter(|p| terms.tiers.contains(p.fee_wad))
        .map(|p| p.used + (p.used >> ABOVE_SHIFT) + U256::from(1))
        .collect();
    probes
        .iter()
        .filter(|p| !above.contains(&p.ask))
        .copied()
        .collect()
}

/// `info` with only `side`'s direction, asked at `seeds`.
fn asking(info: &PoolInfo, side: Side, seeds: BTreeSet<U256>) -> PoolInfo {
    let mut info = info.clone();
    info.directions
        .retain(|plan| (plan.token_in == 0) == (side == Side::Sale));
    info.directions[0].asked = seeds.into_iter().collect();
    info
}

/// `info`'s own seeds on `side`, and `more`.
fn seeds_with(info: &PoolInfo, side: Side, more: &[U256]) -> BTreeSet<U256> {
    info.directions
        .iter()
        .find(|plan| (plan.token_in == 0) == (side == Side::Sale))
        .expect("the side is planned")
        .asked
        .iter()
        .chain(more)
        .copied()
        .collect()
}

/// The sampler's walk of `info`'s one direction at `block`.
async fn walk_at(feed: &FlammFeed, info: &PoolInfo, block: u64) -> DirectionRead {
    let mut walks = feed
        .reader()
        .walks(info, block)
        .await
        .unwrap_or_else(|e| panic!("the sampler walks {}: {e}", info.pool));
    assert_eq!(walks.len(), 1);
    walks.remove(0)
}

fn row_of(words: &Words) -> Row {
    Row {
        spread_wad: words.params[ROW_AT],
        spread_dn_wad: words.params[ROW_AT + 1],
        up_cap_wad: words.params[ROW_AT + 2],
        dn_cap_wad: words.params[ROW_AT + 3],
        band_wad: words.params[ROW_AT + 5],
    }
}

/// The fork's pins on a pool the feed read: its hook's class, its row's words at their offsets,
/// which pass at the live band, a pool of two tokens, and the router as the caller the pool hands
/// its hook. The live band, token 1's.
async fn pins(fork: &Fork, info: &PoolInfo, block: u64) -> U256 {
    let code = fork
        .provider
        .get_code_at(info.hook)
        .await
        .expect("the hook's code");
    assert_eq!(hook_class(&code), HOOK_CLASS, "{}'s hook is of the listed class", info.pool);
    let words = words(fork, info, (CBBTC, USDC), U256::from(100_000u64), block).await;
    let row: Vec<U256> = ROW_WORDS
        .iter()
        .map(|w| U256::from(*w))
        .collect();
    assert_eq!(words.params[ROW_AT..ROW_AT + 8], row[..], "{}'s row words", info.pool);
    assert_eq!(info.row, row_of(&words), "the feed reads the row at its offsets");
    let live_band = U256::from(
        fork.token_config(info.pool, block)
            .await
            .swapPriceBandWad,
    );
    assert!(info.row.passes(live_band), "{}'s row passes at the live band", info.pool);
    assert_eq!(info.trust, Trust::Trusted);
    assert_eq!(info.rule, Rule::Everlong);
    assert_eq!(info.tokens.len(), 2);
    let caller = Address::from_slice(&words.request[0].to_be_bytes::<32>()[12..]);
    assert_eq!(caller, fork.taker, "the caller the pool hands its hook is the router");
    live_band
}

/// One direction checked at a block: its walk settles into the published ladder, whose quotes
/// are the book's, and neither the book nor the rule without the haircut quotes a size of the
/// grid past the pool's swap; its leverage points have their room and a priced point of the
/// rule's first run above them.
struct Checked {
    terms: Terms,
    ladder: Ladder,
    capped: bool,
    words: Words,
    tolerance: U256,
    report: Value,
}

async fn check_direction(
    fork: &Fork,
    pool: &PoolRead,
    walk: &DirectionRead,
    state: &FlammPairState,
    block: u64,
    swaps: &mut Swaps,
) -> Checked {
    let info = &pool.info;
    let (side, pair) = (side_of(walk), pair_of(info, walk));
    let what = format!("{} {side:?}", info.pool);
    let terms = walk.terms(info);
    let settled = settle(&walk.probes, &terms);
    let ladder = book_ladder(state, pair);
    assert_eq!(
        &Ladder::settle(&walk.probes, &terms, HAIRCUT_BPS),
        ladder,
        "{what}: the walk settles into the published ladder"
    );
    let at = walk
        .probes
        .first()
        .map_or_else(|| U256::from(1_000_000u64), |p| p.ask);
    let words = words(fork, info, pair, at, block).await;
    if !walk.band_wad.is_zero() {
        let read = (walk.band_wad, walk.usd_in, walk.usd_out, walk.legs.to_vec());
        let request = &words.request;
        let own = (request[10], request[11], request[12], request[15..23].to_vec());
        assert_eq!(read, own, "{what}: the walk's request words are the pool's");
        if !walk.bid_wad.is_zero() {
            assert_eq!(walk.bid_wad, words.lev[6], "{what}: the walk's bid is levState's word 6");
        }
    }
    let tolerance = tolerance(side, terms.decimals_in, terms.decimals_out, &words);
    let sizes = grid(&[ladder, &settled.ladder], GRID, ACROSS_FLAT);
    fork.swaps(info.pool, pair, &sizes, block, swaps)
        .await;
    let quote = book_quote(state, pair);
    for size in &sizes {
        assert_eq!(quote(*size), ladder.output(*size), "{what}: the book quotes its ladder");
    }
    let booked = Tally::of(&quote, swaps, &sizes);
    let bare = Tally::of(|size| settled.ladder.output(size), swaps, &sizes);
    booked.holds(&format!("{what}, the book"), tolerance);
    bare.holds(&format!("{what}, the rule without the haircut"), tolerance);
    let (tiers, slack) = tier_points(&terms, &walk.probes, &settled.ladder, &what);
    let (labels, flats) = labels(&walk.probes, &settled.ladder);
    let report = json!({
        "pool": info.pool, "side": format!("{side:?}"), "probes": walk.probes.len(),
        "truncated": walk.truncated, "points": settled.ladder.rungs().len(),
        "top": settled.ladder.last().map(|rung| (rung.input, rung.output)),
        "capped": settled.capped, "labels": labels, "flats": flats, "curveFee": walk.curve_fee,
        "bid": walk.bid_wad, "levBid": words.lev[6], "tierPoints": tiers,
        "leastRoomPastMargin": (tiers > 0).then_some(slack), "tolerance": tolerance,
        "sizes": sizes.len(), "book": booked, "bare": bare, "bookBp": booked.bp(),
        "bareBp": bare.bp(),
    });
    Checked { terms, ladder: settled.ladder, capped: settled.capped, words, tolerance, report }
}

/// Where `walk`'s leverage route meets its curve: a coarse scan from its highest
/// leverage-labelled priced probe to the next probe on no tier, its last leverage-labelled size
/// and the next bisected to [`BISECTED`] units, then every size [`DENSE`] either side, previewed.
async fn edge_scan(
    fork: &Fork,
    info: &PoolInfo,
    walk: &DirectionRead,
    terms: &Terms,
    block: u64,
) -> Previews {
    let pair = pair_of(info, walk);
    let tiered = |answer: &Option<(U256, U256, U256)>| {
        answer.is_some_and(|(_, _, label)| terms.tiers.contains(label))
    };
    let leverage = |p: &Probe| p.status == ProbeStatus::Priced && terms.tiers.contains(p.fee_wad);
    let from = walk
        .probes
        .iter()
        .filter(|p| leverage(p))
        .map(|p| p.ask)
        .max()
        .expect("a leverage-labelled priced probe");
    let to = walk
        .probes
        .iter()
        .filter(|p| p.ask > from && !leverage(p))
        .map(|p| p.ask)
        .min()
        .unwrap_or_else(|| from * U256::from(8));
    let coarse = fork
        .previews(info.pool, pair, log_sizes(from, to, SCAN), block)
        .await;
    let mut low = coarse
        .iter()
        .filter(|(_, answer)| tiered(answer))
        .map(|(size, _)| *size)
        .max()
        .expect("the scan reads leverage");
    let mut high = coarse
        .keys()
        .copied()
        .find(|size| *size > low)
        .expect("the scan reads past leverage");
    while high - low > U256::from(BISECTED) {
        let mid = (low + high) >> 1;
        if tiered(
            &fork
                .preview(info.pool, pair, mid, block)
                .await,
        ) {
            low = mid;
        } else {
            high = mid;
        }
    }
    let start = low.saturating_sub(U256::from(DENSE));
    let count = u64::try_from(high + U256::from(DENSE) - start).expect("a short zone");
    fork.previews(info.pool, pair, (0..=count).map(|k| start + U256::from(k)), block)
        .await
}

/// A scan's changes of route, and the sizes the curve prices between two the leverage route
/// prices.
fn zone_of(previews: &Previews, terms: &Terms) -> (Vec<U256>, Vec<U256>) {
    let label = |size: &U256| match previews.get(size) {
        Some(Some((_, _, fee))) if terms.tiers.contains(*fee) => 'T',
        Some(Some(_)) => 'c',
        Some(None) | None => 'x',
    };
    let sizes: Vec<U256> = previews.keys().copied().collect();
    let flips = sizes
        .windows(2)
        .filter(|w| label(&w[0]) != label(&w[1]))
        .map(|w| w[1])
        .collect();
    let one = U256::from(1);
    let holes = sizes
        .iter()
        .copied()
        .filter(|a| label(a) == 'c' && label(&(*a - one)) == 'T' && label(&(*a + one)) == 'T')
        .collect();
    (flips, holes)
}

/// The far edge of `walk`'s leverage route on one pool: a walk from its own seeds with two rungs
/// forced either side of a size the curve prices between two the leverage route prices, and a
/// walk from 32 seeds from a thousandth of a token to a billion tokens, each settled and checked
/// as the pool's own walk is, across where the routes meet too. The control: the forced walk less
/// its probe above, settled with the room test and the probe-above test off, must pass the swap.
async fn far_edge(
    fork: &Fork,
    feed: &FlammFeed,
    pool: &PoolRead,
    walk: &DirectionRead,
    tolerance: U256,
    block: u64,
    swaps: &mut Swaps,
) -> (RecordedEdge, Vec<Value>) {
    let info = &pool.info;
    let (side, pair) = (side_of(walk), pair_of(info, walk));
    let what = format!("{} {side:?} at the far edge", info.pool);
    let terms = walk.terms(info);
    let scan = edge_scan(fork, info, walk, &terms, block).await;
    let (flips, holes) = zone_of(&scan, &terms);
    let hole = *holes
        .first()
        .unwrap_or_else(|| panic!("{what}: no size priced on the curve between two on leverage"));
    let (one, margin) = (U256::from(1), U256::from(200u64));
    let (first, last) = (flips[0], *flips.last().expect("a change of route"));
    let zone: BTreeSet<U256> = scan
        .keys()
        .copied()
        .filter(|size| *size + margin >= first && *size <= last + margin)
        .collect();
    let forced_info = asking(info, side, seeds_with(info, side, &[hole - one, hole + one]));
    let forced = walk_at(feed, &forced_info, block).await;
    let decimals = info.tokens[walk.plan.token_in].decimals;
    let other_info = asking(info, side, sweep(decimals).into_iter().collect());
    let other = walk_at(feed, &other_info, block).await;
    let mut sizes = zone.clone();
    for walked in [&forced, &other] {
        let settled = settle(&walked.probes, &walked.terms(info));
        sizes.extend(grid(&[&settled.ladder], GRID, ACROSS_FLAT));
    }
    fork.swaps(info.pool, pair, &sizes, block, swaps)
        .await;
    let at_hole = BTreeSet::from([hole]);
    let mut reports = Vec::new();
    for (name, walked) in [("forced rungs", &forced), ("32 seeds", &other)] {
        let terms = walked.terms(info);
        let settled = settle(&walked.probes, &terms);
        let haircut = Ladder::settle(&walked.probes, &terms, HAIRCUT_BPS);
        let what = format!("{what}, {name}");
        let bare = Tally::of(|size| settled.ladder.output(size), swaps, &sizes);
        let cut = Tally::of(|size| haircut.output(size), swaps, &sizes);
        bare.holds(&what, tolerance);
        cut.holds(&what, tolerance);
        let on_hole = Tally::of(|size| settled.ladder.output(size), swaps, &at_hole);
        let (tiers, slack) = tier_points(&terms, &walked.probes, &settled.ladder, &what);
        let (labels, flats) = labels(&walked.probes, &settled.ladder);
        reports.push(json!({
            "pool": info.pool, "side": format!("{side:?}"), "walk": name,
            "probes": walked.probes.len(), "truncated": walked.truncated,
            "points": settled.ladder.rungs().len(),
            "top": settled.ladder.last().map(|r| (r.input, r.output)), "labels": labels,
            "flats": flats, "tierPoints": tiers, "leastRoomPastMargin": (tiers > 0).then_some(slack),
            "sizes": sizes.len(), "bare": bare, "haircut": cut, "atHole": on_hole,
        }));
    }
    let forced_terms = forced.terms(info);
    let control = settle_with(
        &less_above(&forced.probes, &forced_terms),
        &forced_terms,
        RoomTest::Off,
        AboveTest::Off,
    );
    let over = Tally::of(|size| control.ladder.output(size), swaps, &zone);
    over.passes(&what, tolerance);
    reports.push(json!({
        "pool": info.pool, "side": format!("{side:?}"), "walk": "control", "hole": hole,
        "zone": [zone.first(), zone.last()], "flips": flips.len(), "holes": holes.len(),
        "points": control.ladder.rungs().len(),
        "top": control.ladder.last().map(|r| (r.input, r.output)), "over": over, "bp": over.bp(),
    }));
    let around = U256::from(RECORD_AROUND_HOLE);
    let kept: BTreeSet<U256> = zone
        .range(hole - around..=hole + around)
        .copied()
        .collect();
    let settled = settle(&forced.probes, &forced_terms);
    let edge = RecordedEdge {
        hole,
        walk: RecordedWalk::of(&forced),
        swaps: recorded_swaps(swaps, &kept, &settled.ladder),
    };
    (edge, reports)
}

#[derive(Serialize, Deserialize)]
struct Recorded {
    block: u64,
    taker: Address,
    pools: Vec<RecordedPool>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedPool {
    pool: Address,
    hook: Address,
    live_band: U256,
    trusted: bool,
    directions: Vec<RecordedDirection>,
    /// The sale's walk with two rungs forced either side of `hole`, which the curve prices between
    /// two sizes the leverage route prices, and the swaps around it.
    edge: Option<RecordedEdge>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedDirection {
    sale: bool,
    decimals_in: u8,
    decimals_out: u8,
    walk: RecordedWalk,
    /// The published ladder: input, output and whether the span ending there is flat.
    rungs: Vec<(U256, U256, bool)>,
    words: Words,
    tolerance: U256,
    /// Sizes swapped whole and their outputs, and sizes the swap refuses.
    swaps: Vec<(U256, Option<U256>)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedWalk {
    curve_fee: U256,
    band_wad: U256,
    usd_in: U256,
    usd_out: U256,
    legs: Vec<U256>,
    bid_wad: U256,
    /// Ask, used, out, label and status: 1 priced, 2 clipped, 3 refused, 4 starved, 5 malformed.
    probes: Vec<(U256, U256, U256, U256, u8)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedEdge {
    hole: U256,
    walk: RecordedWalk,
    swaps: Vec<(U256, Option<U256>)>,
}

fn status_code(status: ProbeStatus) -> u8 {
    match status {
        ProbeStatus::Priced => 1,
        ProbeStatus::Clipped => 2,
        ProbeStatus::Refused => 3,
        ProbeStatus::Starved => 4,
        ProbeStatus::Malformed => 5,
    }
}

fn status_of(code: u8) -> ProbeStatus {
    match code {
        1 => ProbeStatus::Priced,
        2 => ProbeStatus::Clipped,
        3 => ProbeStatus::Refused,
        4 => ProbeStatus::Starved,
        _ => ProbeStatus::Malformed,
    }
}

impl RecordedWalk {
    fn of(walk: &DirectionRead) -> Self {
        Self {
            curve_fee: walk.curve_fee,
            band_wad: walk.band_wad,
            usd_in: walk.usd_in,
            usd_out: walk.usd_out,
            legs: walk.legs.to_vec(),
            bid_wad: walk.bid_wad,
            probes: walk
                .probes
                .iter()
                .map(|p| (p.ask, p.used, p.out, p.fee_wad, status_code(p.status)))
                .collect(),
        }
    }

    fn probes(&self) -> Vec<Probe> {
        self.probes
            .iter()
            .map(|&(ask, used, out, fee_wad, status)| Probe {
                ask,
                used,
                out,
                fee_wad,
                status: status_of(status),
            })
            .collect()
    }

    fn terms(&self, direction: &RecordedDirection, trusted: bool) -> Terms {
        let row = row_of(&direction.words);
        Terms {
            rule: Rule::Everlong,
            tiers: row.tiers(),
            row_band: row.band_wad,
            side: if direction.sale { Side::Sale } else { Side::Buy },
            decimals_in: direction.decimals_in,
            decimals_out: direction.decimals_out,
            trust: if trusted { Trust::Trusted } else { Trust::Untrusted },
            curve_fee: self.curve_fee,
            request: Request {
                band_wad: self.band_wad,
                usd_in: self.usd_in,
                usd_out: self.usd_out,
                legs: self
                    .legs
                    .clone()
                    .try_into()
                    .expect("eight legs"),
                bid_wad: self.bid_wad,
            },
        }
    }
}

/// The swaps of `sizes` a record keeps: whole fills and refusals. A size the swap clips must be
/// one `ladder` does not quote.
fn recorded_swaps(
    swaps: &Swaps,
    sizes: &BTreeSet<U256>,
    ladder: &Ladder,
) -> Vec<(U256, Option<U256>)> {
    sizes
        .iter()
        .filter_map(|size| match swaps.get(size).copied().flatten() {
            None => Some((*size, None)),
            Some((used, out)) if used == *size => Some((*size, Some(out))),
            Some(_) => {
                assert!(ladder.output(*size).is_none(), "a quoted size the swap clips: {size}");
                None
            }
        })
        .collect()
}

fn swaps_of(recorded: &[(U256, Option<U256>)]) -> (Swaps, BTreeSet<U256>) {
    let swaps = recorded
        .iter()
        .map(|&(size, out)| (size, out.map(|out| (size, out))))
        .collect();
    let sizes = recorded
        .iter()
        .map(|(size, _)| *size)
        .collect();
    (swaps, sizes)
}

fn rungs_of(ladder: &Ladder) -> Vec<(U256, U256, bool)> {
    ladder
        .rungs()
        .iter()
        .map(|rung| (rung.input, rung.output, rung.segment == Segment::Flat))
        .collect()
}

fn env_side(name: &str) -> Option<Side> {
    match env::var(name).ok()?.as_str() {
        "sale" => Some(Side::Sale),
        "buy" => Some(Side::Buy),
        other => panic!("{name} is sale or buy, not {other}"),
    }
}

fn env_address(name: &str) -> Address {
    env::var(name)
        .unwrap_or_else(|_| panic!("{name} names a pool"))
        .parse()
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn print(name: &str, reports: &[Value]) {
    println!("{name} {}", serde_json::to_string(reports).expect("reports serialize"));
}

/// A fork where the leverage route wins small sales: on both pools every published ladder is what
/// its walk settles into and quotes as the book does, and neither the book nor the rule without
/// the haircut quotes a size of the grid past the pool's swap; every leverage point has its room
/// and a priced point of the rule's first run above it. With `FLAMM_FORK_EDGE`, that side's far
/// edge and its control too.
#[tokio::test]
#[ignore = "Requires RPC_URL: a local fork of Base where the leverage route wins small sales"]
async fn a_lever_fork_quotes_no_size_past_its_swap() {
    let fork = Fork::connect();
    for pool in POOLS {
        fork.fund(pool).await;
    }
    let block = fork.head().await;
    let feed = feed(&fork.url);
    let (read, books) = poll(&feed, block).await;
    let edge_side = env_side("FLAMM_FORK_EDGE");
    let mut reports = Vec::new();
    let mut recorded = Recorded { block, taker: fork.taker, pools: Vec::new() };
    for pool in POOLS {
        let polled = pool_read(&read, pool);
        let live_band = pins(&fork, &polled.info, block).await;
        let state = published(&books, pool);
        let mut entry = RecordedPool {
            pool,
            hook: polled.info.hook,
            live_band,
            trusted: polled.info.trust == Trust::Trusted,
            directions: Vec::new(),
            edge: None,
        };
        for walk in &polled.directions {
            let pair = pair_of(&polled.info, walk);
            let mut swaps = Swaps::new();
            let checked = check_direction(&fork, polled, walk, state, block, &mut swaps).await;
            reports.push(checked.report.clone());
            if edge_side == Some(side_of(walk)) {
                let (edge, edge_reports) =
                    far_edge(&fork, &feed, polled, walk, checked.tolerance, block, &mut swaps)
                        .await;
                reports.extend(edge_reports);
                entry.edge = Some(edge);
            }
            let ladder = book_ladder(state, pair);
            let sizes = grid(&[ladder], RECORD_GRID, RECORD_ACROSS_FLAT);
            fork.swaps(pool, pair, &sizes, block, &mut swaps)
                .await;
            let direction = RecordedDirection {
                sale: side_of(walk) == Side::Sale,
                decimals_in: checked.terms.decimals_in,
                decimals_out: checked.terms.decimals_out,
                walk: RecordedWalk::of(walk),
                rungs: rungs_of(ladder),
                words: checked.words,
                tolerance: checked.tolerance,
                swaps: recorded_swaps(&swaps, &sizes, ladder),
            };
            assert_eq!(
                direction
                    .walk
                    .terms(&direction, entry.trusted),
                checked.terms,
                "the record rebuilds the walk's terms"
            );
            entry.directions.push(direction);
        }
        recorded.pools.push(entry);
    }
    print("lever", &reports);
    if let Ok(path) = env::var("FLAMM_FORK_RECORD") {
        let text = serde_json::to_string(&recorded).expect("the record serializes");
        fs::write(&path, text + "\n").unwrap_or_else(|e| panic!("{path}: {e}"));
    }
}

/// The record of the lever fork, settled again with no node: each published ladder is its
/// walk's, each tolerance its words', no quote passes a swap, every leverage point has its room
/// and a priced point of the rule's first run above it, and at the far edge the forced walk holds
/// while its control, less the probe above and with the room and probe-above tests off, passes
/// the swap at the hole.
#[test]
fn the_recorded_lever_fork_settles_as_published() {
    let recorded: Recorded = serde_json::from_str(RECORD).expect("the record parses");
    assert_eq!(recorded.taker, router(Chain::Base).expect("Base has a router"));
    let pools: Vec<Address> = recorded
        .pools
        .iter()
        .map(|pool| pool.pool)
        .collect();
    assert_eq!(pools, POOLS);
    let pinned: Vec<U256> = ROW_WORDS
        .iter()
        .map(|w| U256::from(*w))
        .collect();
    let mut leverage_points = 0;
    for pool in &recorded.pools {
        assert!(pool.trusted);
        for direction in &pool.directions {
            let what = format!("{} {}", pool.pool, if direction.sale { "sale" } else { "buy" });
            assert_eq!(direction.words.params[ROW_AT..ROW_AT + 8], pinned[..]);
            assert!(row_of(&direction.words).passes(pool.live_band));
            let terms = direction
                .walk
                .terms(direction, pool.trusted);
            let probes = direction.walk.probes();
            assert_eq!(
                tolerance(terms.side, terms.decimals_in, terms.decimals_out, &direction.words),
                direction.tolerance,
                "{what}: the tolerance of its words"
            );
            let published = Ladder::settle(&probes, &terms, HAIRCUT_BPS);
            assert_eq!(rungs_of(&published), direction.rungs, "{what}: the published ladder");
            let settled = settle(&probes, &terms);
            let (swaps, sizes) = swaps_of(&direction.swaps);
            Tally::of(|size| published.output(size), &swaps, &sizes)
                .holds(&format!("{what}, published"), direction.tolerance);
            Tally::of(|size| settled.ladder.output(size), &swaps, &sizes)
                .holds(&format!("{what}, bare"), direction.tolerance);
            leverage_points += tier_points(&terms, &probes, &settled.ladder, &what).0;
            let Some(edge) = pool
                .edge
                .as_ref()
                .filter(|_| direction.sale)
            else {
                continue;
            };
            let what = format!("{what} at the far edge");
            let terms = edge.walk.terms(direction, pool.trusted);
            let probes = edge.walk.probes();
            let settled = settle(&probes, &terms);
            let (swaps, sizes) = swaps_of(&edge.swaps);
            assert!(sizes.contains(&edge.hole));
            Tally::of(|size| settled.ladder.output(size), &swaps, &sizes)
                .holds(&what, direction.tolerance);
            tier_points(&terms, &probes, &settled.ladder, &what);
            let control =
                settle_with(&less_above(&probes, &terms), &terms, RoomTest::Off, AboveTest::Off);
            Tally::of(|size| control.ladder.output(size), &swaps, &BTreeSet::from([edge.hole]))
                .passes(&what, direction.tolerance);
        }
        assert!(pool.edge.is_some(), "{}: the record holds the far edge", pool.pool);
    }
    assert!(leverage_points > 0, "the record publishes leverage points");
}

/// A sale that clips at the pool's USDC ceiling, at each ceiling in turn. The control is the
/// state's own: a large sale's preview passes its swap where the hook keeps a leverage top, and
/// equals it where it keeps none. Where it keeps one, the ladder is not capped and its top is no
/// leverage-labelled clip; where it keeps none, the ladder is capped on the curve's clip. Either
/// way nothing is quoted past the top and no quote passes the swap.
#[tokio::test]
#[ignore = "Requires RPC_URL: a local fork of Base whose pool FLAMM_FORK_POOL clips a large sale at FLAMM_FORK_CEILINGS"]
async fn a_capped_sale_quotes_no_size_past_its_swap() {
    let fork = Fork::connect();
    let pool = env_address("FLAMM_FORK_POOL");
    let kept_top = env::var("FLAMM_FORK_KEPT_TOP").is_ok();
    let ceilings: Vec<U256> = env::var("FLAMM_FORK_CEILINGS")
        .expect("FLAMM_FORK_CEILINGS lists USDC ceilings, highest first")
        .split(',')
        .map(|c| c.trim().parse().expect("a ceiling"))
        .collect();
    fork.fund(pool).await;
    let feed = feed(&fork.url);
    let (large, sale) = (U256::from(LARGE_SALE), (CBBTC, USDC));
    let mut reports = Vec::new();
    for ceiling in ceilings {
        fork.set_ceiling(pool, ceiling).await;
        let block = fork.head().await;
        let preview = fork
            .preview(pool, sale, large, block)
            .await
            .expect("a large sale's preview");
        let one_pass = fork
            .swap(pool, sale, large, block)
            .await
            .expect("a large sale's swap");
        if kept_top {
            assert!(preview.1 > one_pass.1, "at {ceiling} the preview does not pass the swap");
        } else {
            assert_eq!(preview.1, one_pass.1, "at {ceiling} the preview and the swap differ");
        }
        let (read, books) = poll(&feed, block).await;
        let polled = pool_read(&read, pool);
        pins(&fork, &polled.info, block).await;
        let state = published(&books, pool);
        let walk = walk_of(polled, Side::Sale);
        let mut swaps = Swaps::new();
        let checked = check_direction(&fork, polled, walk, state, block, &mut swaps).await;
        let top = checked
            .ladder
            .last()
            .expect("a sale ladder");
        let top_probe = probe_at(&walk.probes, top.input);
        let clip = top_probe.status == ProbeStatus::Clipped;
        let tier_clip = clip &&
            checked
                .terms
                .tiers
                .contains(top_probe.fee_wad);
        if kept_top {
            assert!(!checked.capped, "at {ceiling}: capped");
            assert!(!tier_clip, "at {ceiling}: a leverage-labelled clip at the top");
        } else {
            assert!(checked.capped, "at {ceiling}: not capped");
            assert!(clip && !tier_clip, "at {ceiling}: the top is not the curve's clip");
        }
        let quote = book_quote(state, sale);
        let multiples = [1.0001, 1.001, 1.01, 1.05, 1.2, 2.0, 5.0, 100.0, 1_000.0, 10_000.0];
        let above: Vec<U256> = std::iter::once(top.input + U256::from(1))
            .chain(
                multiples
                    .iter()
                    .map(|m| floor(float(wide(top.input)) * m)),
            )
            .collect();
        for size in &above {
            assert!(quote(*size).is_none(), "at {ceiling}: quoted past the top at {size}");
            assert!(checked.ladder.output(*size).is_none());
        }
        let mut report = checked.report;
        report["ceiling"] = json!(ceiling);
        report["preview"] = json!(preview);
        report["swap"] = json!(one_pass);
        report["previewOverSwapBp"] =
            json!((float(wide(preview.1)) / float(wide(one_pass.1)) - 1.0) * 1e4);
        report["topProbe"] = json!((
            top_probe.ask,
            top_probe.used,
            top_probe.fee_wad,
            status_code(top_probe.status)
        ));
        report["unquotedAboveTop"] = json!(above.len());
        reports.push(report);
    }
    print("capped", &reports);
}

/// A buy at the lever-down's join: `levState`'s bid word reads zero, so the walk publishes no
/// leverage-labelled point and no quote passes the swap. A walk with rungs forced either side of
/// where the leverage route's output falls does the same. Control: those rungs, settled as though
/// the bid word were positive, publish leverage points across the fall and pass the swap.
#[tokio::test]
#[ignore = "Requires RPC_URL: a local fork of Base at the lever-down's join on FLAMM_FORK_POOL"]
async fn a_buy_at_the_join_quotes_nothing_on_leverage() {
    let fork = Fork::connect();
    let pool = env_address("FLAMM_FORK_POOL");
    fork.fund(pool).await;
    let block = fork.head().await;
    let feed = feed(&fork.url);
    let (read, books) = poll(&feed, block).await;
    let polled = pool_read(&read, pool);
    pins(&fork, &polled.info, block).await;
    let state = published(&books, pool);
    let walk = walk_of(polled, Side::Buy);
    assert!(!walk.band_wad.is_zero(), "the buy read its request: a leverage label shows");
    assert!(walk.bid_wad.is_zero(), "the walk reads a bid at the join");
    let mut swaps = Swaps::new();
    let checked = check_direction(&fork, polled, walk, state, block, &mut swaps).await;
    assert!(checked.words.lev[6].is_zero(), "levState's bid word reads zero at the join");
    let terms = &checked.terms;
    let (tiers, _) = tier_points(terms, &walk.probes, &checked.ladder, "the join");
    assert_eq!(tiers, 0, "the join publishes a leverage-labelled point");
    let pair = pair_of(&polled.info, walk);
    let from = walk
        .probes
        .iter()
        .filter(|p| p.status == ProbeStatus::Priced && terms.tiers.contains(p.fee_wad))
        .map(|p| p.used)
        .max()
        .expect("a leverage-labelled priced probe");
    let coarse = fork
        .previews(pool, pair, log_sizes(from, from * U256::from(4), SCAN), block)
        .await;
    let leveraged: Vec<(U256, U256)> = coarse
        .iter()
        .filter_map(|(size, answer)| {
            answer.and_then(|(used, out, fee)| {
                (terms.tiers.contains(fee) && used == *size).then_some((*size, out))
            })
        })
        .collect();
    let fall = leveraged
        .windows(2)
        .position(|w| w[1].1 < w[0].1)
        .expect("the leverage route's output falls");
    let (mut low, mut high, peak) = (leveraged[fall].0, leveraged[fall + 1].0, leveraged[fall].1);
    while high - low > U256::from(1) {
        let mid = (low + high) >> 1;
        let answer = fork
            .preview(pool, pair, mid, block)
            .await;
        if answer.is_some_and(|(_, out, _)| out >= peak) {
            low = mid;
        } else {
            high = mid;
        }
    }
    let join = low;
    let answers: Vec<_> = fork
        .previews(pool, pair, [join, join + U256::from(1)], block)
        .await
        .into_values()
        .collect();
    let joined = answers[0].map_or(peak, |(_, out, _)| out);
    let past = leveraged[fall + 1..]
        .iter()
        .find(|(_, out)| *out >= joined)
        .map(|(size, _)| *size)
        .expect("the leverage route's output passes its height at the join again");
    let forced_seeds = seeds_with(&polled.info, Side::Buy, &[join, past]);
    let forced = walk_at(&feed, &asking(&polled.info, Side::Buy, forced_seeds), block).await;
    let forced_terms = forced.terms(&polled.info);
    let natural = settle(&forced.probes, &forced_terms);
    let mut bid = forced_terms.clone();
    bid.request.bid_wad = U256::from(1);
    let control = settle(&forced.probes, &bid);
    let mut sizes = grid(&[&natural.ladder, &control.ladder], GRID, ACROSS_FLAT);
    sizes.extend((0..ACROSS_FLAT).map(|k| join + U256::from(k)));
    fork.swaps(pool, pair, &sizes, block, &mut swaps)
        .await;
    let held = Tally::of(|size| natural.ladder.output(size), &swaps, &sizes);
    let (forced_tiers, _) =
        tier_points(&forced_terms, &forced.probes, &natural.ladder, "the join, forced");
    let control_tiers = control
        .ladder
        .rungs()
        .iter()
        .filter(|rung| {
            terms
                .tiers
                .contains(probe_at(&forced.probes, rung.input).fee_wad)
        })
        .count();
    let over = Tally::of(|size| control.ladder.output(size), &swaps, &sizes);
    let mut report = checked.report;
    report["join"] = json!({
        "at": join, "peak": peak, "joined": joined, "past": past, "answers": answers,
    });
    report["forced"] = json!({
        "probes": RecordedWalk::of(&forced).probes, "points": natural.ladder.rungs().len(),
        "held": held,
    });
    report["control"] = json!({
        "rungs": rungs_of(&control.ladder), "tierPoints": control_tiers, "over": over,
        "bp": over.bp(),
    });
    print("join", &[report]);
    assert!(forced.bid_wad.is_zero());
    held.holds("the join, forced rungs", checked.tolerance);
    assert_eq!(forced_tiers, 0, "the forced walk publishes a leverage-labelled point");
    assert!(control_tiers > 0, "the control publishes no leverage-labelled point");
    over.passes("the join", checked.tolerance);
}

/// A sale with a USDC ceiling between two outputs of the leverage route, so that the hole sells on
/// the curve between two sizes that sell on leverage. Neither the pool's own walk, nor a walk from
/// 32 seeds from a thousandth of a token to a billion tokens, nor one with rungs forced either
/// side of the hole, its probe above included, quotes the hole or a size of the grid past the
/// swap. Control: the forced rungs with nothing above them, settled with the probe-above test off,
/// publish and pass the swap at the hole. Taking out the probe above alone is not enough: the
/// walk's next seed clips on the curve at the ceiling, and the flat span up to that clip lowers
/// the last leverage point to the ray through it, under the swap at the hole.
///
/// The forced rungs and the walk's probe above, with a starved and then a refused answer put
/// halfway between the upper rung and that probe, publish no leverage point and quote nothing
/// past the swap: the starved answer leaves no ladder and the refused one the probe above alone.
/// Control: each of those sets, settled with the probe-above test off, publishes the rungs and
/// passes the swap at the hole.
#[tokio::test]
#[ignore = "Requires RPC_URL: a local fork of Base with a USDC ceiling inside the leverage route of FLAMM_FORK_POOL at FLAMM_FORK_HOLE"]
async fn a_token_ceiling_hole_is_not_quoted_over() {
    let fork = Fork::connect();
    let pool = env_address("FLAMM_FORK_POOL");
    let hole: U256 = env::var("FLAMM_FORK_HOLE")
        .expect("FLAMM_FORK_HOLE names the size between two on leverage")
        .parse()
        .expect("a size");
    fork.fund(pool).await;
    let block = fork.head().await;
    let feed = feed(&fork.url);
    let (read, books) = poll(&feed, block).await;
    let polled = pool_read(&read, pool);
    pins(&fork, &polled.info, block).await;
    let state = published(&books, pool);
    let walk = walk_of(polled, Side::Sale);
    let mut swaps = Swaps::new();
    let checked = check_direction(&fork, polled, walk, state, block, &mut swaps).await;
    let (one, sale) = (U256::from(1), (CBBTC, USDC));
    let around = [hole - one, hole, hole + one];
    let answers = fork
        .previews(pool, sale, around, block)
        .await;
    for (k, size) in around.iter().enumerate() {
        let (_, _, label) = answers[size].expect("a preview at the hole");
        let leverage = checked.terms.tiers.contains(label);
        assert_eq!(leverage, k != 1, "leverage, curve, leverage at {size}");
    }
    let forced_seeds = seeds_with(&polled.info, Side::Sale, &[hole - one, hole + one]);
    let forced = walk_at(&feed, &asking(&polled.info, Side::Sale, forced_seeds), block).await;
    let forced_terms = forced.terms(&polled.info);
    let rule = settle(&forced.probes, &forced_terms);
    let unbacked =
        |probes: &[Probe]| settle_with(probes, &forced_terms, RoomTest::Margin, AboveTest::Off);
    let less = unbacked(&less_above(&forced.probes, &forced_terms));
    let bare: Vec<Probe> = forced
        .probes
        .iter()
        .filter(|p| p.ask <= hole + one)
        .copied()
        .collect();
    let control = unbacked(&bare);
    let forced_at = |ask: U256| {
        *forced
            .probes
            .iter()
            .find(|p| p.ask == ask)
            .unwrap_or_else(|| panic!("the forced walk asks {ask}"))
    };
    let (lower, upper) = (forced_at(hole - one), forced_at(hole + one));
    let above = forced_at(upper.used + (upper.used >> ABOVE_SHIFT) + one);
    let mid = upper.ask + (above.ask - upper.ask) / U256::from(2);
    let starved = Probe {
        ask: mid,
        used: U256::ZERO,
        out: U256::ZERO,
        fee_wad: U256::ZERO,
        status: ProbeStatus::Starved,
    };
    let injected: Vec<Injected> = [
        ("starved", starved, Vec::new()),
        ("refused", Probe::classify(mid, None), vec![(above.used, above.out, false)]),
    ]
    .into_iter()
    .map(|(answer, cut, want)| {
        let probes = vec![lower, upper, cut, above];
        let rule = settle(&probes, &forced_terms);
        let control = unbacked(&probes);
        Injected { answer, probes, rule, control, want }
    })
    .collect();
    let decimals = polled.info.tokens[walk.plan.token_in].decimals;
    let seeded = asking(&polled.info, Side::Sale, sweep(decimals).into_iter().collect());
    let seeded = walk_at(&feed, &seeded, block).await;
    let seeded_terms = seeded.terms(&polled.info);
    let seeds = settle(&seeded.probes, &seeded_terms);
    let mut ladders = vec![&rule.ladder, &seeds.ladder, &control.ladder, &less.ladder];
    for cut in &injected {
        ladders.extend([&cut.rule.ladder, &cut.control.ladder]);
    }
    let mut sizes = grid(&ladders, GRID, ACROSS_FLAT);
    sizes.extend(around);
    fork.swaps(pool, sale, &sizes, block, &mut swaps)
        .await;
    let at_hole = BTreeSet::from([hole]);
    let natural = Tally::of(|size| checked.ladder.output(size), &swaps, &at_hole);
    let held = Tally::of(|size| rule.ladder.output(size), &swaps, &sizes);
    let held_at = Tally::of(|size| rule.ladder.output(size), &swaps, &at_hole);
    let seeds_held = Tally::of(|size| seeds.ladder.output(size), &swaps, &sizes);
    let seeds_at = Tally::of(|size| seeds.ladder.output(size), &swaps, &at_hole);
    let less_at = Tally::of(|size| less.ladder.output(size), &swaps, &at_hole);
    let over = Tally::of(|size| control.ladder.output(size), &swaps, &at_hole);
    let cuts: Vec<(Tally, Tally)> = injected
        .iter()
        .map(|cut| {
            let held = Tally::of(|size| cut.rule.ladder.output(size), &swaps, &sizes);
            let over = Tally::of(|size| cut.control.ladder.output(size), &swaps, &at_hole);
            (held, over)
        })
        .collect();
    let mut report = checked.report;
    report["hole"] = json!({
        "at": hole, "answers": around.iter().map(|s| answers[s]).collect::<Vec<_>>(),
        "ownWalk": natural,
    });
    report["forced"] = json!({
        "probes": RecordedWalk::of(&forced).probes, "rungs": rungs_of(&rule.ladder),
        "held": held, "atHole": held_at,
    });
    report["seeds"] = json!({
        "probes": RecordedWalk::of(&seeded).probes, "rungs": rungs_of(&seeds.ladder),
        "held": seeds_held, "atHole": seeds_at,
    });
    report["lessAbove"] = json!({
        "rungs": rungs_of(&less.ladder), "atHole": less_at, "bp": less_at.bp(),
    });
    report["control"] = json!({
        "rungs": rungs_of(&control.ladder), "over": over, "bp": over.bp(),
    });
    report["injected"] = json!(injected
        .iter()
        .zip(&cuts)
        .map(|(cut, (held, over))| json!({
            "answer": cut.answer, "at": mid, "rungs": rungs_of(&cut.rule.ladder), "held": held,
            "control": { "rungs": rungs_of(&cut.control.ladder), "over": over, "bp": over.bp() },
        }))
        .collect::<Vec<_>>());
    print("token ceiling", &[report]);
    natural.holds("the hole, the pool's own walk", checked.tolerance);
    held.holds("the hole, forced rungs", checked.tolerance);
    tier_points(&forced_terms, &forced.probes, &rule.ladder, "the hole, forced rungs");
    seeds_held.holds("the hole, 32 seeds", checked.tolerance);
    tier_points(&seeded_terms, &seeded.probes, &seeds.ladder, "the hole, 32 seeds");
    over.passes("the hole", checked.tolerance);
    for (cut, (held, over)) in injected.iter().zip(&cuts) {
        let what = format!("the hole, a {} answer above the upper rung", cut.answer);
        let tiers = leverage_points(&forced_terms, &cut.probes, &cut.rule.ladder);
        assert_eq!(tiers, 0, "{what}: a leverage-labelled point publishes");
        assert_eq!(rungs_of(&cut.rule.ladder), cut.want, "{what}: the rungs it leaves");
        held.holds(&what, checked.tolerance);
        let control_tiers = leverage_points(&forced_terms, &cut.probes, &cut.control.ladder);
        assert!(control_tiers > 0, "{what}: the control publishes no leverage-labelled point");
        over.passes(&what, checked.tolerance);
    }
}

/// The forced rungs either side of a hole and the walk's probe above the upper one, with one
/// answer put between the upper rung and that probe, settled by the rule and, as its control,
/// with the probe-above test off.
struct Injected {
    answer: &'static str,
    probes: Vec<Probe>,
    rule: Settled,
    control: Settled,
    /// The rungs the rule leaves, as [`rungs_of`] gives them.
    want: Vec<(U256, U256, bool)>,
}

/// A quantity of a node's answer, as a number.
fn quantity(value: &Value) -> u64 {
    value
        .as_str()
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or_else(|| panic!("a quantity: {value}"))
}

/// What `receipt`'s logs show of `token` sent to `to`.
fn transferred(receipt: &Value, token: Address, to: Address) -> U256 {
    let topic = keccak256("Transfer(address,address,uint256)");
    let word = |value: &Value| -> Option<B256> { value.as_str()?.parse().ok() };
    receipt["logs"]
        .as_array()
        .expect("a receipt's logs")
        .iter()
        .filter(|log| {
            log["address"]
                .as_str()
                .and_then(|a| a.parse::<Address>().ok()) ==
                Some(token)
        })
        .filter(|log| {
            let topics = log["topics"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            topics.len() == 3 &&
                word(&topics[0]) == Some(topic) &&
                word(&topics[2]) == Some(to.into_word())
        })
        .map(|log| {
            let data = log["data"]
                .as_str()
                .expect("a log's data");
            U256::from_str_radix(data.trim_start_matches("0x"), 16).expect("a transfer's amount")
        })
        .fold(U256::ZERO, |sum, amount| sum + amount)
}

/// A transaction's intrinsic gas: 21,000 and its calldata's, 4 a zero byte and 16 another.
fn intrinsic_gas(data: &[u8]) -> u64 {
    21_000 +
        data.iter()
            .map(|byte| if *byte == 0 { 4 } else { 16 })
            .sum::<u64>()
}

/// The feed over the BSC fork's factory and price feed and the hooks' class, BTCB and USDT its
/// tokens, the node's block taken as it is, charging swaps by the measured table.
fn bsc_feed(url: &str, class: B256) -> FlammFeed {
    let (btcb, usdt) = (Bytes::from(BTCB.to_vec()), Bytes::from(USDT.to_vec()));
    let config = BookFeedConfig {
        chain: Chain::Bsc,
        tokens: Arc::new(test_token_map(&[(&btcb, "BTCB", 18), (&usdt, "USDT", 18)])),
        min_tvl_usd: 0.0,
    };
    FlammFeedBuilder::new(config, url.to_string())
        .factories(vec![Bytes::from(BSC_FACTORY.to_vec())])
        .hook_classes(vec![(class, HookShape::Everlong)])
        .price_feeds(vec![Bytes::from(BSC_PRICE_FEED.to_vec())])
        .haircut_bps(HAIRCUT_BPS)
        .max_block_lag(None)
        .feed_config(HttpFeedConfig {
            request_timeout: Duration::from_secs(120),
            ..default_http_feed_config()
        })
        .build()
        .expect("the feed builds")
}

/// The inputs a published ladder is checked at: each rung's, and the middle of each span.
fn checked_sizes(ladder: &Ladder) -> BTreeSet<U256> {
    let inputs: Vec<U256> = ladder
        .rungs()
        .iter()
        .map(|rung| rung.input)
        .collect();
    let middles = inputs
        .windows(2)
        .map(|pair| pair[0] + (pair[1] - pair[0]) / U256::from(2u64));
    inputs
        .iter()
        .copied()
        .chain(middles)
        .collect()
}

/// On BSC, the feed serves the pools of every kind there, Aave V3, Venus Core and Lista Moolah;
/// and every input it publishes either way, after a deposit, after a borrowing sale and after a
/// buy, fills at least its quote through the pool's own swap from the router, at a gas net of the
/// refund, less what the route saves inside the pool's frame, at or over its charge. Printed per
/// pool and side: whether a walk read the legs, the charges and the worst under-charge.
#[tokio::test]
#[ignore = "Requires RPC_URL: a local fork of BSC with the factory BSC_FACTORY and its three pools at their seeds"]
async fn a_bsc_fork_charges_no_published_fill_over_its_gas() {
    let fork = Fork::connect_to(Chain::Bsc);
    let class = fork
        .hook_class_of(BSC_AAVE_POOL, fork.head().await)
        .await;
    assert!(
        gas_limit_addition(Chain::Bsc, class, 1, true).is_some(),
        "the measured table holds the hooks' class {class}"
    );
    for pool in BSC_DEPOSITED {
        let deposit = depositProRataCall {
            poolAssets: U256::from(BSC_DEPOSIT),
            receiver: BSC_LP,
            maxLoanAssetIn: vec![U256::MAX >> 1],
            minLoanAssetOut: vec![U256::ZERO],
            minSharesOut: U256::ZERO,
            deadline: U256::MAX >> 1,
        };
        fork.send(BSC_LP, pool, deposit.abi_encode())
            .await;
    }
    let feed = bsc_feed(&fork.url, class);
    let mut reports = Vec::new();
    let mut checked = 0usize;
    for stage in ["deposited", "after a borrowing sale", "after a buy"] {
        let block = fork.head().await;
        let (read, books) = poll(&feed, block).await;
        let served: BTreeSet<Address> = read
            .pools
            .iter()
            .map(|p| p.info.pool)
            .collect();
        assert_eq!(served, BTreeSet::from(BSC_SERVED), "{stage}: the pools the feed reads");
        for pool in BSC_SERVED {
            let Some(state) = book_of(&books, pool) else {
                reports.push(json!({ "stage": stage, "pool": pool, "book": false }));
                continue;
            };
            let legs_read = pool_read(&read, pool)
                .directions
                .iter()
                .any(|walk| !walk.band_wad.is_zero());
            for pair in [(BTCB, USDT), (USDT, BTCB)] {
                let side = if pair.0 == BTCB { "sale" } else { "buy" };
                let (token_in, token_out) = if state.base_token.address == *pair.0.as_slice() {
                    (&state.base_token, &state.quote_token)
                } else {
                    (&state.quote_token, &state.base_token)
                };
                let sizes = checked_sizes(book_ladder(state, pair));
                fork.warm(pool, pair, &sizes).await;
                let (mut worst, mut least_spare) = (0u64, u64::MAX);
                let mut charges = BTreeSet::new();
                for size in &sizes {
                    let amount = BigUint::from_bytes_be(&size.to_be_bytes::<32>());
                    let Ok(quote) = state.get_amount_out(amount, token_in, token_out) else {
                        continue;
                    };
                    let (out, net) = fork
                        .fill_found(pool, pair, *size, false)
                        .await
                        .unwrap_or_else(|| {
                            panic!("{stage}, {pool} {side} of {size}: a quote whose swap reverts")
                        });
                    let quoted = biguint_to_u256(&quote.amount);
                    assert!(
                        out >= quoted,
                        "{stage}, {pool} {side} of {size}: quoted {quoted}, filled {out}"
                    );
                    let charge = u64::try_from(quote.gas.clone()).expect("a charge in u64");
                    let real = net - BSC_ROUTE_SAVING;
                    assert!(
                        charge <= real,
                        "{stage}, {pool} {side} of {size}: charged {charge}, costs {real}"
                    );
                    worst = worst.max(real - charge);
                    least_spare = least_spare.min(real - charge);
                    charges.insert(charge);
                    checked += 1;
                }
                reports.push(json!({
                    "stage": stage, "pool": pool, "side": side, "sizes": sizes.len(),
                    "legs_read": legs_read, "charges": charges,
                    "worst_under_charge": worst, "least_under_charge": least_spare,
                }));
            }
        }
        for pool in BSC_SERVED {
            let Some(state) = book_of(&books, pool) else { continue };
            let pair = if stage == "deposited" { (BTCB, USDT) } else { (USDT, BTCB) };
            let rungs = book_ladder(state, pair).rungs();
            if let Some(middle) = rungs.get(rungs.len() / 2) {
                assert!(
                    fork.fill_found(pool, pair, middle.input, true)
                        .await
                        .is_some(),
                    "{stage}: the move of {pool}"
                );
            }
        }
    }
    print("bsc fills against their charge", &reports);
    assert!(checked > 0);
}
