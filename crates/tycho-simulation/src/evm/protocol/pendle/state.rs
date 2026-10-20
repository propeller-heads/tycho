//! `PendleState` — one `ProtocolSim` covering both component types.
//!
//! Tycho binds a single state type per protocol system, but the Substreams package emits two kinds
//! of component: `pendle_market` (the SY↔PT AMM) and `pendle_sy` (the ERC-5115 wrapper). So the
//! state is an enum that dispatches on `protocol_type_name` at decode time and forwards every
//! `ProtocolSim` method.
//!
//! The six edges the brief asks for split across the two:
//!
//! | Edge | Component | Shape |
//! |---|---|---|
//! | PT → SY | market | exact, closed form |
//! | SY → PT | market | inverted by the router's search |
//! | YT → SY | market | exact, via the flash-swap identity |
//! | SY → YT | market | inverted by the router's search |
//! | underlying → SY | sy | constant rate, per token class |
//! | SY → underlying | sy | constant rate, per token class |
//!
//! The YT legs are flash-swaps against the **same reserves** as the PT legs. They are quoted on the
//! market component, and `get_limits` reports each direction's own bound rather than sharing one.
//!
//! Every market quote returns the state the trade would leave behind — the reserves it moves and
//! the implied rate the next trade is anchored on — so a caller sizing or splitting a trade prices
//! each leg against the depth the one before it left.

use std::{any::Any, collections::HashMap, fmt, fmt::Debug, str::FromStr, sync::OnceLock};

use alloy::primitives::{I256, U256};
use hex_literal::hex;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{
    clock,
    math::{
        approx,
        errors::PendleError,
        market::{self, MarketPreCompute, MarketState},
        pmath, sy_utils,
    },
};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64};

/// How far the PY index may move between the sampled and the execution block while a market
/// limit still holds: 1e-6, as a fraction of [`ONE_E18`]. A yield token accruing 3% a year moves
/// the index about 1.1e-8 a block, so this covers some 90 blocks of ordinary accrual, or one block
/// at any plausible rate. A discrete jump past it, such as a rebase report landing between the two
/// blocks, is not covered.
const INDEX_DRIFT: U256 = U256::from_limbs([1_000_000_000_000, 0, 0, 0]);
const ONE_E18: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The share of a market edge's bound its limit gives up once it must hold at the execution block,
/// as a fraction of [`ONE_E18`]: 1%.
///
/// The limit is not wei-exact there, on purpose. Next to a bound, whether a trade fills is not
/// monotone in the index: the router's search, the anchor's `ln` and `exp`, and the SY↔asset
/// conversions all round, and a limit within about 1e-3 of its bound filled at the checked indices
/// and reverted at some index between them. No finite set of checked indices covers the rest, so
/// the limit stays far enough inside that the rounding cannot reach it. A trade a hundredth short
/// of a market's whole depth already moves its rate to the floor or the cap, so the share given up
/// is depth no route would use.
const EXECUTION_HAIRCUT: U256 = U256::from_limbs([10_000_000_000_000_000, 0, 0, 0]);

/// The rounding in a market's rate anchor that a bound at the exchange-rate floor must absorb
/// once it must hold at the execution block, beyond [`EXECUTION_HAIRCUT`]: 1e5 wei of
/// [`ONE_E18`]. The anchor is recomputed from the index through `ln` and `exp`, which round, and
/// the floor's PT bound, `total_pt − p·(total_pt + total_asset)` with `p` the sigmoid of
/// `(fee − anchor)·rate_scalar`, moves by about
/// `total_pt·total_asset / (total_pt + total_asset) · rate_scalar` per unit of anchor. Near the fee
/// floor, where the bound is a small difference of large reserves, that is most of it. Probes
/// found errors of a few thousand wei on markets within 1e-13 of the floor; the allowance is
/// forty times that. Away from the floor the exchange rate clears it by billions of wei even a
/// second before expiry, so it costs nothing there. See
/// [`PendleMarketState::clear_of_anchor_noise`].
const ANCHOR_NOISE: U256 = U256::from_limbs([100_000, 0, 0, 0]);

/// Gas for a swap routed through Router V4, on a market whose SY is not in
/// [`MARKET_GAS_SURCHARGE`].
///
/// Measured per edge on a mainnet fork at block 25 800 000 against the wstETH market, each edge in
/// its own test on a fresh fork, then rounded up by roughly a tenth. A sweep of all 69 live markets
/// at block 26 228 000 put these at about the 75th percentile of markets. The headroom is
/// deliberate and asymmetric in its consequences: under-estimating makes a router pick a route that
/// costs more than it was quoted, while over-estimating only makes Pendle look slightly worse than
/// it is.
///
/// The two directions of a leg are **not** one number. Buying PT costs 40% more than selling it,
/// and buying YT 27% more than selling it, because the exact-SY-in directions run the router's
/// approximation loop on chain. Charging the dearer figure to both would misprice the cheaper
/// direction badly enough to change routing decisions.
///
/// | Edge | Measured | Charged |
/// |---|---|---|
/// | SY to PT | 301,220 | 330,000 |
/// | PT to SY | 215,674 | 235,000 |
/// | SY to YT | 387,038 | 425,000 |
/// | YT to SY | 305,972 | 335,000 |
/// | wrap, `one_to_one` | 80,671 / 78,047 | 125,000 |
/// | wrap, `index_rate` (stETH in / out, WETH in) | 164,759 / 152,397 / 207,260 | 230,000 |
const GAS_SY_FOR_PT: u64 = 330_000;
const GAS_PT_FOR_SY: u64 = 235_000;
const GAS_SY_FOR_YT: u64 = 425_000;
const GAS_YT_FOR_SY: u64 = 335_000;

/// A `one_to_one` wrap is the cheapest case there is: the SY takes the token and mints, and does
/// nothing else. What "nothing else" costs depends on the token's own transfer: across the live
/// SYs at block 26 228 000 the median was 93k and the 90th percentile 122k.
const GAS_WRAP_ONE_TO_ONE: u64 = 125_000;

/// An `index_rate` wrap reads through to whatever protocol the SY wraps — an ERC-4626 vault, a
/// staking contract — and costs whatever that costs. Measured on the wstETH SY: its stETH legs,
/// which wrap and unwrap through wstETH, take 152k–165k, and a WETH deposit, which also unwraps
/// the WETH and stakes it with Lido, 207k. Another wrapped protocol can cost more or less. The
/// class is known at quote time, so the two classes are charged separately.
const GAS_WRAP_INDEX_RATE: u64 = 230_000;

/// Extra gas on every market edge of a market whose SY is listed, on top of the edge's constant.
///
/// Some SYs make every transfer expensive: transfer hooks, reward accounting, or the protocol
/// they wrap. Every market edge moves SY, so the extra cost belongs to the SY, and a sweep of the
/// 69 live markets at block 26 228 000 found it nearly the same on all four edges of a market
/// and across trade sizes. Each value is the SY's worst sample plus a tenth, less the edge's
/// constant, rounded up to 5,000. Without these, a route through one of these markets is
/// quoted up to half a million gas short.
///
/// Hardcoded, and so only as current as that sweep: an SY that turns heavy, or a heavy one that
/// launches after it, is charged the plain constant until it is added here. How to keep the table
/// current is open.
const MARKET_GAS_SURCHARGE: [([u8; 20], u64); 15] = [
    // SY-mixWETH
    (hex!("3870cf5e2eab623a74d2af1007295963b1c5a12f"), 535_000),
    // SY-sUSDfr
    (hex!("a6e23d9a6f7f4fb5ec305b50b29418dd119eb692"), 425_000),
    // SY-sBOLD
    (hex!("a73fb4f3a3f3889e41387973afc542fa06785981"), 280_000),
    // SY-mixUSDC
    (hex!("0e012d6a77a7f6c91de7cb74c2732553876d1d61"), 215_000),
    // SY-ROY-JT-apyUSD
    (hex!("464a2b93b6633707470bcd2c81d34519f38e1e9c"), 200_000),
    // SY-ROY-ST-apyUSD
    (hex!("a99a50d95003e7687d7962f29aad6cdd3655d411"), 195_000),
    // SY-srUSDat
    (hex!("9d0fc59f88acb85520a8cfb74c7fe141d8563185"), 165_000),
    // SY-sUSDat
    (hex!("8917f8c7feb840b5837edc7e128123baa2f289f9"), 90_000),
    // SY-srnOPAL
    (hex!("efc86eda0d06f6c608f75e4bc918d079c6779cec"), 90_000),
    // SY-jrUSDe
    (hex!("4f6673346ab4813f1665327ab39087008cc7d76f"), 75_000),
    // SY-srUSDe
    (hex!("c9bfebc79a722c05dc34bd2a227ef2db19fd1b8e"), 75_000),
    // SY pufETH
    (hex!("253008ba4ae2f3e6488dc998a5321d4eb1a0c905"), 65_000),
    // SY-siUSD
    (hex!("9f30507c264cc6eb5be35b18ff9ad7b4539aa920"), 40_000),
    // SY-sthUSD
    (hex!("beefe990f4099e11c6ab745555621cdfa161f18d"), 30_000),
    // SY-mAPOLLO
    (hex!("e1e22d55e23900f30599d8bee74a0801f09b4e58"), 30_000),
];

/// Extra gas on a deposit or redemption of a listed `(SY, token)` pair, on top of its class's
/// constant.
///
/// An `index_rate` wrap costs whatever the wrapped protocol costs, which differs by token: SY-siUSD
/// pays out USDC through a redemption queue at 2.29M gas, while its other wraps cost a fraction of
/// that. Measured on every wrap of the live markets' SYs at block 26 228 000, and set like
/// [`MARKET_GAS_SURCHARGE`] from the worse of the two directions. The same caveat applies: the
/// table is only as current as that sweep.
const WRAP_GAS_SURCHARGE: [([u8; 20], [u8; 20], u64); 23] = [
    // SY-siUSD, USDC
    (
        hex!("9f30507c264cc6eb5be35b18ff9ad7b4539aa920"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        2_290_000,
    ),
    // SY-sUSDfr, USDfr
    (
        hex!("a6e23d9a6f7f4fb5ec305b50b29418dd119eb692"),
        hex!("cc07e7c4e5e35affd47b351e420a22c667d7f83d"),
        1_535_000,
    ),
    // SY-mixWETH, WETH
    (
        hex!("3870cf5e2eab623a74d2af1007295963b1c5a12f"),
        hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
        1_380_000,
    ),
    // SY-siUSD, iUSD
    (
        hex!("9f30507c264cc6eb5be35b18ff9ad7b4539aa920"),
        hex!("48f9e38f3070ad8945dfeae3fa70987722e3d89c"),
        795_000,
    ),
    // SY-mixUSDC, USDC
    (
        hex!("0e012d6a77a7f6c91de7cb74c2732553876d1d61"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        665_000,
    ),
    // SY-sBOLD, BOLD
    (
        hex!("a73fb4f3a3f3889e41387973afc542fa06785981"),
        hex!("6440f144b7e50d6a8439336510312d2f54beb01d"),
        595_000,
    ),
    // SY-USD3, USDC
    (
        hex!("ea3bc608f32847b97965c5e1648bdfcd4c2c40d0"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        415_000,
    ),
    // SY-srUSDat, USDat
    (
        hex!("9d0fc59f88acb85520a8cfb74c7fe141d8563185"),
        hex!("23238f20b894f29041f48d88ee91131c395aaa71"),
        385_000,
    ),
    // SY-srUSDe, USDe
    (
        hex!("c9bfebc79a722c05dc34bd2a227ef2db19fd1b8e"),
        hex!("4c9edd5852cd905f086c759e8383e09bff1e68b3"),
        270_000,
    ),
    // SY-jrUSDe, USDe
    (
        hex!("4f6673346ab4813f1665327ab39087008cc7d76f"),
        hex!("4c9edd5852cd905f086c759e8383e09bff1e68b3"),
        255_000,
    ),
    // SY-fxSAVE, fxSP
    (
        hex!("13945b761b2ed3219a497a46d15a8923f418d2ab"),
        hex!("65c9a641afceb9c0e6034e558a319488fa0fa3be"),
        225_000,
    ),
    // SY-fxSAVE, fxSP
    (
        hex!("7ca334286e9519de99703ab49d4eff6e5559499f"),
        hex!("65c9a641afceb9c0e6034e558a319488fa0fa3be"),
        225_000,
    ),
    // SY-mAPOLLO, USDC
    (
        hex!("e1e22d55e23900f30599d8bee74a0801f09b4e58"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        165_000,
    ),
    // SY-mAPOLLO, USDS
    (
        hex!("e1e22d55e23900f30599d8bee74a0801f09b4e58"),
        hex!("dc035d45d973e3ec169d2276ddab16f1e407384f"),
        150_000,
    ),
    // SY-sUSDfr, sUSDfr
    (
        hex!("a6e23d9a6f7f4fb5ec305b50b29418dd119eb692"),
        hex!("af559d1d59b33ca4b950ab2091372af8a773e234"),
        130_000,
    ),
    // SY-sUSD3, USD3
    (
        hex!("e78392ecb40939c1c29d9d47429913dd88b5daed"),
        hex!("056b269eb1f75477a8666ae8c7fe01b64dd55ecc"),
        125_000,
    ),
    // SY-sUSDS, DAI
    (
        hex!("be3d4ec488a0a042bb86f9176c24f8cd54018ba7"),
        hex!("6b175474e89094c44da98b954eedeac495271d0f"),
        75_000,
    ),
    // SY-sUSDat, USDat
    (
        hex!("8917f8c7feb840b5837edc7e128123baa2f289f9"),
        hex!("23238f20b894f29041f48d88ee91131c395aaa71"),
        65_000,
    ),
    // SY SIERRA, USDC
    (
        hex!("9845bbb51d326fe4d5e7cbd47ce157a33d7936ef"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        60_000,
    ),
    // SY-sAID, USDC
    (
        hex!("39fb0bb5d696ec6da2d827649cf3a3f00299d2dd"),
        hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        45_000,
    ),
    // SY SIERRA, SIERRA
    (
        hex!("9845bbb51d326fe4d5e7cbd47ce157a33d7936ef"),
        hex!("bceb5f6877d979ec621ae694da1102cb95691ad3"),
        35_000,
    ),
    // SY-ROY-JT-apyUSD, ROY-JT-apyUSD
    (
        hex!("464a2b93b6633707470bcd2c81d34519f38e1e9c"),
        hex!("ab2ab53e1e2e2c5d7202918ec8c873712bcc4a2d"),
        20_000,
    ),
    // SY-ROY-ST-apyUSD, ROY-ST-apyUSD
    (
        hex!("a99a50d95003e7687d7962f29aad6cdd3655d411"),
        hex!("bd373c9d3d8976a4fecc504a93c768bbe8c3227c"),
        20_000,
    ),
];

/// The extra gas [`MARKET_GAS_SURCHARGE`] lists for a market on `sy`; zero for any other.
fn market_gas_surcharge(sy: &Bytes) -> u64 {
    MARKET_GAS_SURCHARGE
        .iter()
        .find(|(address, _)| sy.as_ref() == address)
        .map_or(0, |(_, gas)| *gas)
}

/// The extra gas [`WRAP_GAS_SURCHARGE`] lists for wrapping `token` through `sy`; zero for any
/// other.
fn wrap_gas_surcharge(sy: &Bytes, token: &Bytes) -> u64 {
    WRAP_GAS_SURCHARGE
        .iter()
        .find(|(listed_sy, listed_token, _)| {
            sy.as_ref() == listed_sy && token.as_ref() == listed_token
        })
        .map_or(0, |(_, _, gas)| *gas)
}

/// How an SY converts a given token, as classified at component creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenClass {
    /// One token unit is one SY unit, modulo decimals. A real wrapper.
    OneToOne,
    /// Converted at the SY's exchange rate, like an ERC-4626 share.
    IndexRate,
}

impl TokenClass {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        match raw {
            b"one_to_one" => Some(TokenClass::OneToOne),
            b"index_rate" => Some(TokenClass::IndexRate),
            _ => None,
        }
    }
}

/// The AMM. Everything here comes from attributes the Substreams package emits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleMarketState {
    pub market: MarketState,
    /// `max(pyIndexStored, SY.exchangeRate())` as of [`Self::rate_sampled_at`].
    pub py_index: U256,
    /// The block timestamp at which `py_index` was read: the one moment for which the rate and the
    /// curve describe the same state. Quotes run on this clock until [`Self::execution_timestamp`]
    /// moves them later; see [`Self::quote_time`].
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it, injected by the decoder rather than emitted
    /// by the Substreams package. Only ever compared against `rate_sampled_at`, to tell whether
    /// this state still answers for the current block.
    pub head_timestamp: u64,
    pub sy_address: Bytes,
    pub pt_address: Bytes,
    pub yt_address: Bytes,
    /// Whether the market's SY is paused. A paused SY reverts every transfer, and every market
    /// edge moves SY, so this refuses every quote against the market.
    pub sy_paused: bool,
    /// Timestamp of the block a quote is expected to execute in, from
    /// [`ProtocolSim::apply_block`]; zero until one is given. Once later than
    /// [`Self::rate_sampled_at`], quotes evaluate the curve's time terms here, with the index as
    /// sampled, and a quote that would execute at or after expiry is refused.
    #[serde(default)]
    pub execution_timestamp: u64,
    /// Whether the stream has not yet carried the market's fees, which the indexer reads at
    /// creation and can fail to. The market then refuses every quote until a fee update arrives.
    #[serde(default)]
    pub fee_unread: bool,
    /// Whether the original market factory (`0x27b1…0784`) created this market. Its markets lack
    /// the `MarketZeroNetLPFee` check every later factory's carry, so a trade whose fee to the LPs
    /// rounds to zero fills there and reverts everywhere else.
    #[serde(default)]
    pub from_original_factory: bool,
    /// What this state derives from itself, kept after first use. See [`MarketMemo`].
    #[serde(skip)]
    pub memo: MarketMemo,
}

/// Values a market state derives from itself, kept after their first use: the curve's pre-compute,
/// which every quote and bound starts from, and the limits of each edge, which `spot_price` and
/// `query_pool_swap` ask for again on every call.
///
/// Each value is stored with the state it was computed from and used only while that still
/// matches, so a state whose public fields are changed in place recomputes rather than answering
/// stale. A clone starts empty, and `delta_transition` and `apply_block` empty it, so the state a
/// change leaves behind can fill it again. Equality and serialization ignore it.
#[derive(Default)]
pub struct MarketMemo {
    pre_compute: OnceLock<Box<KeptPreCompute>>,
    limits: [OnceLock<Box<KeptLimits>>; 4],
}

/// A pre-compute, with the market, index and time it was computed at.
type KeptPreCompute = (MarketState, U256, u64, MarketPreCompute);

/// An edge's limits, with the state they were computed from.
type KeptLimits = (PendleMarketState, (BigUint, BigUint));

impl Clone for MarketMemo {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl fmt::Debug for MarketMemo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MarketMemo")
    }
}

impl PartialEq for MarketMemo {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for MarketMemo {}

/// The ERC-5115 wrapper, and the conversions it supports per token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleSyState {
    pub sy_address: Bytes,
    /// Accounting-asset units per SY, as of `rate_sampled_at`.
    pub exchange_rate: U256,
    /// The block timestamp at which `exchange_rate` was read.
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it. See [`PendleMarketState::head_timestamp`].
    pub head_timestamp: u64,
    pub sy_decimals: u32,
    pub asset_decimals: u32,
    /// Entry tokens and how each converts. A token absent here is one the indexer could not
    /// classify, and is not quotable.
    pub tokens_in: HashMap<Bytes, TokenClass>,
    /// Exit tokens and how each converts.
    pub tokens_out: HashMap<Bytes, TokenClass>,
    /// SY shares outstanding. No redemption can take more, whatever the SY holds or where: some
    /// SYs keep their yield token in a staking contract and hold none of it themselves.
    pub total_supply: U256,
    /// Whether the SY is paused, which refuses every deposit and redemption.
    pub paused: bool,
    /// Decimals of each entry and exit token, taken from the stream's token set at decode time.
    ///
    /// Every bound on a wrap edge relates an amount of the underlying token to an amount of SY,
    /// and the two are only comparable through this. A token missing here is one the stream does
    /// not carry, and its edges report no depth rather than a bound scaled by a guess.
    pub token_decimals: HashMap<Bytes, u32>,
    /// The most SY shares the SY lets exist, when it caps its own supply. A deposit that would
    /// mint past it reverts with `SupplyCapExceeded`.
    #[serde(default)]
    pub supply_cap: Option<U256>,
    /// The token the SY holds as its backing (`yieldToken()`), when the stream carries it.
    #[serde(default)]
    pub yield_token: Option<Bytes>,
    /// The SY's own balance of [`Self::yield_token`], from the component balances the stream
    /// reports. It follows transfers, so a rebasing yield token's balance lags its rebases.
    #[serde(default)]
    pub yield_token_held: Option<U256>,
    /// Whether the SY pays a redemption to its yield token out of what it holds of it, judged from
    /// the stream's state (see `PendleSyState::judge_holding`) and kept across quotes.
    #[serde(default)]
    pub holding_backs_supply: bool,
    /// The step a deposit of each entry token mints in, where the SY rounds its mint: USDC into
    /// SY-PPLUS mints a multiple of 10^12 SY wei. Absent where the mint is not rounded.
    #[serde(default)]
    pub deposit_steps: HashMap<Bytes, U256>,
    /// The entry fee, in basis points, the vault behind this SY charges on `index_rate` deposits
    /// from [`Self::entry_fee_from`]; zero for an SY that pays none.
    #[serde(default)]
    pub entry_fee_bps: u32,
    /// The timestamp from which [`Self::entry_fee_bps`] applies, until the vault moves it.
    #[serde(default)]
    pub entry_fee_from: u64,
    /// Timestamp of the block a quote is expected to execute in, from
    /// [`ProtocolSim::apply_block`]; zero until one is given. Only an SY with an entry fee needs
    /// it, to judge whether the fee applies there.
    #[serde(default)]
    pub execution_timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendleState {
    Market(PendleMarketState),
    Sy(PendleSyState),
}

impl PendleMarketState {
    /// The contract's `getMarketPreCompute` at [`Self::quote_time`], kept in [`MarketMemo`].
    fn pre_compute(&self) -> Result<MarketPreCompute, PendleError> {
        let at = self.quote_time();
        if let Some((market, index, time, comp)) = self
            .memo
            .pre_compute
            .get()
            .map(|kept| &**kept)
        {
            if *market == self.market && *index == self.py_index && *time == at {
                return Ok(comp.clone());
            }
        }
        let comp = market::get_market_pre_compute(&self.market, self.py_index, at)?;
        let _ = self.memo.pre_compute.set(Box::new((
            self.market.clone(),
            self.py_index,
            at,
            comp.clone(),
        )));
        Ok(comp)
    }

    /// Which of the market's three tokens an address names.
    fn role(&self, token: &Bytes) -> Option<MarketToken> {
        if *token == self.sy_address {
            Some(MarketToken::Sy)
        } else if *token == self.pt_address {
            Some(MarketToken::Pt)
        } else if *token == self.yt_address {
            Some(MarketToken::Yt)
        } else {
            None
        }
    }

    fn expired(&self) -> bool {
        market::is_expired(self.market.expiry, self.latest_known_time())
    }

    fn guard_live(&self) -> Result<(), SimulationError> {
        if self.expired() {
            return Err(PendleError::MarketExpired {
                expiry: self.market.expiry,
                block_time: self.latest_known_time(),
            }
            .into());
        }
        Ok(())
    }

    /// Refuses every edge while the market's fees are unknown: any quote would price the wrong fee.
    /// Recoverable, since a fee update brings the market back.
    fn guard_fee_read(&self) -> Result<(), SimulationError> {
        if self.fee_unread {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle market {} has no fee yet; it quotes once the stream carries one",
                self.pt_address
            )));
        }
        Ok(())
    }

    /// Refuses every edge while the SY is paused: the swap would revert moving SY. Recoverable,
    /// since an `Unpaused` brings the market back.
    fn guard_sy_unpaused(&self) -> Result<(), SimulationError> {
        if self.sy_paused {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY {} is paused, so its market cannot move SY",
                self.sy_address
            )));
        }
        Ok(())
    }

    /// PT in → SY out. Exact: the market has this primitive. Also returns the LPs' share of the
    /// fee, in SY.
    fn pt_for_sy(&self, pt_in: U256) -> Result<(U256, MarketState, U256), PendleError> {
        let executed = market::execute_trade_with(
            &self.market,
            &self.pre_compute()?,
            self.py_index,
            -pmath::to_i256(pt_in)?,
            self.quote_time(),
        )?;
        Ok((
            pmath::to_u256(executed.trade.net_sy_to_account)?,
            executed.market,
            lp_fee(&executed.trade)?,
        ))
    }

    /// YT in → SY out. A flash-swap: borrow PT, redeem PT+YT to SY, repay the market.
    ///
    /// Redeeming `n` YT needs `n` PT, so the borrowed leg is exactly the YT amount. What the
    /// trader keeps is the redemption proceeds less what repaying the borrow costs. Also returns
    /// the LPs' share of the borrowed leg's fee, in SY.
    fn yt_for_sy(&self, yt_in: U256) -> Result<(U256, MarketState, U256), PendleError> {
        // The borrowed PT leg is a real market trade, so it goes through the same checks the
        // market runs: above all, it cannot lend out more PT than it holds.
        let executed = market::execute_trade_with(
            &self.market,
            &self.pre_compute()?,
            self.py_index,
            pmath::to_i256(yt_in)?,
            self.quote_time(),
        )?;
        // Buying the PT back costs this much SY.
        let sy_owed = pmath::to_u256(-executed.trade.net_sy_to_account)?;
        // The router's callback redeems in two parts, as the contract does: just enough PY,
        // rounded up, to pay the market what it is owed, and the rest to the trader. Redeeming
        // the whole position at once and subtracting the debt after floors only once, and lands
        // a wei above what the trader receives.
        let py_to_repay = sy_utils::sy_to_asset_up(self.py_index, sy_owed)?;
        if yt_in < py_to_repay {
            return Err(PendleError::YtSaleCannotRepay {
                yt_in: yt_in.to_string(),
                py_to_repay: py_to_repay.to_string(),
            });
        }
        let sy_out = sy_utils::asset_to_sy(self.py_index, yt_in - py_to_repay)?;
        // Only the borrowed PT leg touches the market. Tokenizing and redeeming PY happens on the
        // YT contract, against reserves this component does not hold.
        Ok((sy_out, executed.market, lp_fee(&executed.trade)?))
    }

    /// One market edge, quoted: the output, its gas, the reserves the trade leaves behind, and the
    /// LPs' share of its fee in asset units.
    fn amount_out(
        &self,
        amount_in: U256,
        from: MarketToken,
        to: MarketToken,
    ) -> Result<MarketSwap, PendleError> {
        let (amount_out, gas, market, lp_fee_sy) = match (from, to) {
            (MarketToken::Pt, MarketToken::Sy) => {
                let (out, market, lp_fee) = self.pt_for_sy(amount_in)?;
                (out, GAS_PT_FOR_SY, market, lp_fee)
            }
            (MarketToken::Yt, MarketToken::Sy) => {
                let (out, market, lp_fee) = self.yt_for_sy(amount_in)?;
                (out, GAS_YT_FOR_SY, market, lp_fee)
            }
            (MarketToken::Sy, MarketToken::Pt) => {
                let filled = approx::approx_swap_exact_sy_for_pt_with(
                    &self.market,
                    &self.pre_compute()?,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?;
                let lp_fee = filled
                    .net_sy_fee
                    .saturating_sub(filled.net_sy_to_reserve);
                (filled.amount_out, GAS_SY_FOR_PT, filled.market_after, lp_fee)
            }
            (MarketToken::Sy, MarketToken::Yt) => {
                let filled = approx::approx_swap_exact_sy_for_yt_with(
                    &self.market,
                    &self.pre_compute()?,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?;
                // The search only picks how much PT to sell back. The router's callback then
                // mints PY from every SY the YT contract holds, the input plus what the PT sale
                // paid out, so the trader gets at least the guess and usually a little more.
                let net_sy_out = pmath::to_u256(filled.net_sy_to_account)?;
                let sy_to_tokenize = amount_in
                    .checked_add(net_sy_out)
                    .ok_or(PendleError::Overflow { operation: "SY to tokenize" })?;
                let yt_out = sy_utils::sy_to_asset(self.py_index, sy_to_tokenize)?;
                let lp_fee = filled
                    .net_sy_fee
                    .saturating_sub(filled.net_sy_to_reserve);
                (yt_out, GAS_SY_FOR_YT, filled.market_after, lp_fee)
            }
            _ => unreachable!("unsupported pair is rejected before reaching here"),
        };
        let lp_fee = sy_utils::sy_to_asset(self.py_index, lp_fee_sy)?;
        Ok(MarketSwap { amount_out, gas, market, lp_fee })
    }

    /// Refuses a quoted trade the market would revert after computing it: on every market but the
    /// original factory's, `PendleMarketV7` reverts with `MarketZeroNetLPFee` when the LPs' share
    /// of the fee converts to zero asset units. Dust trades hit it, and near expiry, where the
    /// fee rate falls toward zero, so do trades of a few units of a 6-decimal token.
    fn guard_lp_fee(&self, swap: &MarketSwap) -> Result<(), PendleError> {
        if !self.from_original_factory && swap.lp_fee.is_zero() {
            return Err(PendleError::MarketZeroNetLpFee);
        }
        Ok(())
    }

    /// The same state carrying the market a trade left behind.
    ///
    /// The rate and the clocks come across unchanged: a swap moves reserves, it does not advance
    /// the block or re-read the PY index, so the successor answers for the same moment as the
    /// state it came from.
    fn after(&self, market: MarketState) -> Self {
        PendleMarketState { market, ..self.clone() }
    }
}

/// What one market edge resolves to.
struct MarketSwap {
    amount_out: U256,
    gas: u64,
    /// The reserves and implied rate after the trade. A second quote against this prices at the
    /// depth the first one left, which is what makes splitting a trade across sizes meaningful.
    market: MarketState,
    /// The LPs' share of the trade's fee, in asset units, rounded down as the market rounds it.
    lp_fee: U256,
}

/// The LPs' share of a trade's fee, in SY: the whole fee less the treasury's cut.
fn lp_fee(trade: &market::TradeResult) -> Result<U256, PendleError> {
    let lp_share = trade
        .net_sy_fee
        .checked_sub(trade.net_sy_to_reserve)
        .ok_or(PendleError::Overflow { operation: "LP fee" })?;
    pmath::to_u256(lp_share)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarketToken {
    Sy,
    Pt,
    Yt,
}

impl PendleSyState {
    fn class(&self, token_in: &Bytes, token_out: &Bytes) -> Option<(TokenClass, Direction)> {
        if *token_in == self.sy_address {
            return self
                .tokens_out
                .get(token_out)
                .map(|class| (*class, Direction::Redeem));
        }
        if *token_out == self.sy_address {
            return self
                .tokens_in
                .get(token_in)
                .map(|class| (*class, Direction::Deposit));
        }
        None
    }

    /// Underlying → SY, or SY → underlying, at the class's conversion.
    ///
    /// `OneToOne` still rescales for decimals: one wstETH is one SY-wstETH, but only because both
    /// carry 18. A 6-decimal token against an 18-decimal SY is one-to-one in *units*, not in wei.
    ///
    /// An `index_rate` deposit divides by `exchangeRate() + 1`, not the rate itself. The rate is
    /// the SY's price rounded down, while its `previewDeposit` uses the wrapped vault's exact
    /// price, which lies between the two; an SY whose rate carries few digits, such as mixUSDC's
    /// seven, mints up to 1e-6 less than the rate alone predicts. The quote so never promises more
    /// than the SY mints, and is at most one part in the rate low. Redemptions multiply by the
    /// rounded-down rate, which already errs that way.
    ///
    /// A deposit pays the vault's entry fee where one applies (see [`Self::entry_fee_applies`]),
    /// taken from the input rounded up, as the vault takes it, and its output is rounded down to
    /// the token's deposit step.
    fn convert(
        &self,
        amount: U256,
        class: TokenClass,
        direction: Direction,
        token: &Bytes,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        let out = match (class, direction) {
            (TokenClass::OneToOne, Direction::Deposit) => {
                rescale(amount, token_decimals, self.sy_decimals)?
            }
            (TokenClass::OneToOne, Direction::Redeem) => {
                return rescale(amount, self.sy_decimals, token_decimals);
            }
            (TokenClass::IndexRate, Direction::Deposit) => {
                let amount = self.net_of_entry_fee(amount);
                let as_asset = rescale(amount, token_decimals, self.asset_decimals)?;
                sy_utils::asset_to_sy(
                    self.exchange_rate
                        .saturating_add(U256::from(1u8)),
                    as_asset,
                )?
            }
            (TokenClass::IndexRate, Direction::Redeem) => {
                let as_asset = sy_utils::sy_to_asset(self.exchange_rate, amount)?;
                return rescale(as_asset, self.asset_decimals, token_decimals);
            }
        };
        Ok(match self.deposit_steps.get(token) {
            Some(step) if *step > U256::from(1u8) => out - out % *step,
            _ => out,
        })
    }

    /// Whether the vault's entry fee applies at the moment a quote is for: the later of the block
    /// the state was read at and the execution block, when one is recorded.
    fn entry_fee_applies(&self) -> bool {
        let at = self
            .head_timestamp
            .max(self.rate_sampled_at)
            .max(self.execution_timestamp);
        self.entry_fee_bps > 0 && at >= self.entry_fee_from
    }

    /// `amount` less the vault's entry fee where it applies, the fee rounded up.
    fn net_of_entry_fee(&self, amount: U256) -> U256 {
        if !self.entry_fee_applies() {
            return amount;
        }
        let fee = (amount.saturating_mul(U256::from(self.entry_fee_bps)) + U256::from(9_999u32)) /
            U256::from(10_000u32);
        amount.saturating_sub(fee)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Deposit,
    Redeem,
}

fn rescale(amount: U256, from_decimals: u32, to_decimals: u32) -> Result<U256, PendleError> {
    if from_decimals == to_decimals {
        return Ok(amount);
    }
    if from_decimals < to_decimals {
        let factor = U256::from(10)
            .checked_pow(U256::from(to_decimals - from_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        amount
            .checked_mul(factor)
            .ok_or(PendleError::Overflow { operation: "rescale" })
    } else {
        let factor = U256::from(10)
            .checked_pow(U256::from(from_decimals - to_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        Ok(amount / factor)
    }
}

#[typetag::serde]
impl ProtocolSim for PendleState {
    /// The market's fee, as a ratio. It is a rate-space multiplier on chain, so this converts it
    /// to the amount-space number the trait documents, and it decays to zero at expiry.
    fn fee(&self) -> f64 {
        match self {
            PendleState::Market(state) => {
                let Ok(comp) = state.pre_compute() else {
                    return 0.0;
                };
                let excess = comp.fee_rate - pmath::i_one();
                let Ok(excess) = pmath::to_u256(excess) else { return 0.0 };
                u256_to_f64(excess).unwrap_or(0.0) / 1e18
            }
            // A wrapper takes no fee of its own for the classes quoted here.
            PendleState::Sy(_) => 0.0,
        }
    }

    /// The price of `base` in `quote`, from the buying side as the trait defines it: what a small
    /// trade of `quote` into `base` pays per unit.
    ///
    /// Not every component trades both ways. An SY can accept a token it never pays out, and a
    /// redemption the SY holds nothing for has no depth, while the other direction trades. When
    /// buying `base` does not quote, the price comes from selling it instead, as `quote` received
    /// per unit of `base`. A wrap edge charges no fee, so the two sides give the same price.
    ///
    /// Priced off a real trade rather than a closed form, so the number agrees with what a caller
    /// would actually get, fee included.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        match self.marginal_rate(quote, base) {
            Ok(base_per_quote) => Ok(1.0 / base_per_quote),
            Err(buying) => self
                .marginal_rate(base, quote)
                .map_err(|_| buying),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.quote(amount_in, token_in, token_out, true)
    }

    /// The largest trade that will not revert, per direction.
    ///
    /// Each market direction has its own binding constraint and they do not follow from one
    /// another: SY→PT is bounded by the router search's rate floor, SY→YT by the search's bound at
    /// the proportion cap, and PT→SY by the 96% proportion cap itself; each is the largest input
    /// that fills. YT→SY is the trader's output peak, short of where the sale stops filling.
    ///
    /// Without an execution block these are exact. With one recorded, each limit gives up 1% of
    /// its bound (`EXECUTION_HAIRCUT`) and must fill at that block with the index as sampled and
    /// `INDEX_DRIFT` above it: near a bound, rounding makes filling flip with the index, so an
    /// exact limit there is not reliable a block later. An edge whose limit is dust, or whose
    /// bound a checked state cannot evaluate, reports no depth; selling YT never evaluates its
    /// bound, so there the boundary is searched for instead.
    ///
    /// A pair this component does not trade — PT against YT, or a token that is not one of its
    /// three — reports **zero depth rather than an error**. A caller enumerating the pairs of a
    /// component is asking a question, and "none" is the answer; only an actual swap request is
    /// wrong enough to fail. The integration test enumerates every pair, PT↔YT included.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        match self {
            PendleState::Market(state) => {
                let slot = match (state.role(&sell_token), state.role(&buy_token)) {
                    (Some(MarketToken::Sy), Some(MarketToken::Pt)) => Some(0),
                    (Some(MarketToken::Pt), Some(MarketToken::Sy)) => Some(1),
                    (Some(MarketToken::Sy), Some(MarketToken::Yt)) => Some(2),
                    (Some(MarketToken::Yt), Some(MarketToken::Sy)) => Some(3),
                    _ => None,
                };
                let cached = slot
                    .and_then(|slot| state.memo.limits[slot].get())
                    .filter(|cached| cached.0 == *state);
                if let Some(cached) = cached {
                    return Ok(cached.1.clone());
                }
                let limits = market_limits(state, sell_token, buy_token)?;
                if let Some(slot) = slot {
                    let _ = state.memo.limits[slot].set(Box::new((state.clone(), limits.clone())));
                }
                Ok(limits)
            }
            PendleState::Sy(state) => {
                let Some((class, direction)) = state.class(&sell_token, &buy_token) else {
                    return Ok(no_depth());
                };
                // As for a market: a rate this state cannot quote with bounds nothing. A
                // `one_to_one` edge never reads the rate, so only a paused SY stops it.
                if state.guard_quotable(class).is_err() {
                    return Ok(no_depth());
                }
                // The underlying token's own decimals, never the SY's. Both bounds below relate a
                // token amount to an SY amount, and substituting `sy_decimals` makes each wrong by
                // `10^|difference|` on any pair whose decimals do not already match.
                let token = match direction {
                    Direction::Deposit => &sell_token,
                    Direction::Redeem => &buy_token,
                };
                let Some(token_decimals) = state.token_decimals.get(token).copied() else {
                    return Ok(no_depth());
                };
                let convert =
                    |amount: U256| state.convert(amount, class, direction, token, token_decimals);
                let max_in = match direction {
                    // A capped SY mints only up to its cap. An uncapped one has no limit of its
                    // own; what the protocol it wraps accepts is not visible here.
                    Direction::Deposit => {
                        let unbounded = unbounded_soft_limit();
                        match state.deposit_headroom() {
                            None => unbounded,
                            Some(room) => largest_input_within(room, unbounded, convert)?,
                        }
                    }
                    Direction::Redeem => state.max_redeem_sy(token, class, token_decimals)?,
                };
                let max_out = convert(max_in)?;
                // An input too small to produce anything is no depth, not a trade.
                if max_out.is_zero() {
                    return Ok(no_depth());
                }
                Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
            }
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        match self {
            PendleState::Market(state) => {
                state.memo = MarketMemo::default();
                state.apply(&delta)
            }
            // An SY's redemption to its yield token is bounded by what it holds of it, which
            // arrives as a component balance.
            PendleState::Sy(state) => {
                state.apply(&delta)?;
                state.apply_balances(balances);
                state.judge_holding();
                Ok(())
            }
        }
    }

    /// Records the execution block's timestamp on a market, so its quotes are evaluated at the
    /// moment the swap runs, and one that would execute at or after expiry is refused. Re-emits
    /// when that moves the quote time or flips the market to expired. An SY's quotes depend on
    /// time only through an entry fee, so it re-emits only when the fee starts or stops applying.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let state = match self {
            PendleState::Market(state) => state,
            PendleState::Sy(sy) => {
                if sy.entry_fee_bps == 0 {
                    return false;
                }
                let before = sy.entry_fee_applies();
                sy.execution_timestamp = block.timestamp();
                return before != sy.entry_fee_applies();
            }
        };
        if state.execution_timestamp == block.timestamp() {
            return false;
        }
        let before = (state.quote_time(), state.expired());
        state.execution_timestamp = block.timestamp();
        state.memo = MarketMemo::default();
        before != (state.quote_time(), state.expired())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<PendleState>()
            .is_some_and(|other| self == other)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

impl PendleState {
    /// [`ProtocolSim::get_amount_out`], with the market's zero-LP-fee refusal optional.
    ///
    /// A quote for a router enforces it, since the market reverts there. A probe for the marginal
    /// rate does not: it prices a small trade to read the curve's slope, and near expiry on a
    /// 6-decimal market even one unit's fee to the LPs rounds to zero.
    fn quote(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
        enforce_lp_fee: bool,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::from(0u32) {
            return Err(SimulationError::InvalidInput("amount_in is zero".to_string(), None));
        }
        // More than any token's supply. The math is checked, but an amount this size overflows
        // inside it, and an overflow there reads as a broken state rather than a request too large.
        if amount_in > BigUint::from(u128::MAX) {
            return Err(SimulationError::InvalidInput(
                format!("amount_in {amount_in} exceeds any token supply"),
                None,
            ));
        }
        let amount = biguint_to_u256(&amount_in);

        let (out, gas, new_state) = match self {
            PendleState::Market(state) => {
                state.guard_live()?;
                state.guard_sy_unpaused()?;
                state.guard_fee_read()?;
                state.guard_current()?;
                let from = state
                    .role(&token_in.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                let to = state
                    .role(&token_out.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                // PT↔YT never touches the market: minting and redeeming PY is the YT's business,
                // and SY is one side of every market edge.
                if from == to || (from != MarketToken::Sy && to != MarketToken::Sy) {
                    return Err(unknown_pair(token_in, token_out));
                }
                let swap = state.amount_out(amount, from, to)?;
                if enforce_lp_fee {
                    state.guard_lp_fee(&swap)?;
                }
                let gas = swap.gas + market_gas_surcharge(&state.sy_address);
                (swap.amount_out, gas, PendleState::Market(state.after(swap.market)))
            }
            PendleState::Sy(state) => {
                let (class, direction) = state
                    .class(&token_in.address, &token_out.address)
                    .ok_or_else(|| unquotable_pair(token_in, token_out))?;
                state.guard_quotable(class)?;
                let token_decimals = match direction {
                    Direction::Deposit => token_in.decimals,
                    Direction::Redeem => token_out.decimals,
                };
                let token = match direction {
                    Direction::Deposit => &token_in.address,
                    Direction::Redeem => &token_out.address,
                };
                let gas = match class {
                    TokenClass::OneToOne => GAS_WRAP_ONE_TO_ONE,
                    TokenClass::IndexRate => GAS_WRAP_INDEX_RATE,
                } + wrap_gas_surcharge(&state.sy_address, token);
                if direction == Direction::Redeem {
                    let max_in = state.max_redeem_sy(&token_out.address, class, token_decimals)?;
                    if amount > max_in {
                        return Err(SimulationError::InvalidInput(
                            format!(
                                "redeeming {amount} SY exceeds the {max_in} the SY can pay out"
                            ),
                            None,
                        ));
                    }
                }
                let out = state.convert(amount, class, direction, token, token_decimals)?;
                if direction == Direction::Deposit {
                    if let Some(room) = state.deposit_headroom() {
                        if out > room {
                            return Err(SimulationError::InvalidInput(
                                format!(
                                    "minting {out} SY passes the supply cap, which leaves {room}"
                                ),
                                None,
                            ));
                        }
                    }
                }
                let after = state.after(direction, token, amount, out)?;
                (out, gas, PendleState::Sy(after))
            }
        };

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(out),
            gas: BigUint::from(gas),
            new_state: Box::new(new_state),
        })
    }

    /// Units of `token_out` a small trade returns per unit of `token_in`.
    ///
    /// Probes with a thousandth of the depth, capped at one unit: small enough to stay inside the
    /// limit on a shallow market and to leave out most of the price impact, large enough that the
    /// output is not lost to rounding. A direction reporting no depth is probed with one unit and
    /// answers if the quote does: an SY's conversion rate exists even when it holds nothing to pay.
    fn marginal_rate(&self, token_in: &Token, token_out: &Token) -> Result<f64, SimulationError> {
        let one_unit = BigUint::from(10u32).pow(token_in.decimals);
        let (max_in, _) = self.get_limits(token_in.address.clone(), token_out.address.clone())?;
        let within_depth = max_in / 1000u32;
        let probe =
            if within_depth == BigUint::from(0u32) { one_unit } else { one_unit.min(within_depth) };
        let quoted = self.quote(probe.clone(), token_in, token_out, false)?;
        let amount_in = biguint_to_f64(&probe) / 10f64.powi(token_in.decimals as i32);
        let amount_out = biguint_to_f64(&quoted.amount) / 10f64.powi(token_out.decimals as i32);
        if amount_out == 0.0 {
            return Err(SimulationError::FatalError(format!(
                "{probe} of {} returns no {}, so there is no rate",
                token_in.address, token_out.address
            )));
        }
        Ok(amount_out / amount_in)
    }
}

impl PendleMarketState {
    /// The timestamp this market's quotes are evaluated at: the execution block's, once
    /// [`ProtocolSim::apply_block`] has recorded one later than the sample, and the block the rate
    /// was read at otherwise.
    ///
    /// The curve's time terms are closed forms, so they can be evaluated at the moment the swap
    /// runs; the index stays the sampled one, which nothing here projects. A quote at the
    /// execution block is therefore what the router pays there if the index has not moved and no
    /// earlier transaction in that block trades the market. Never the chain head alone: whether
    /// the rate still holds for the head is [`Self::is_current`]'s question — see the `clock`
    /// module.
    pub fn quote_time(&self) -> u64 {
        self.rate_sampled_at
            .max(self.execution_timestamp)
    }

    /// Whether this state still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    /// The latest moment this state has evidence of, from either clock.
    ///
    /// Expiry is judged against this rather than against [`Self::quote_time`], which is the rate's
    /// own date. A rate that stopped resolving before the market expired would otherwise leave
    /// `quote_time` frozen below `expiry` and the market never seen to die. The head is a real
    /// chain timestamp, so taking the later of the two can only move the verdict up to where the
    /// chain already is — never past it, and never onto a market that still trades.
    ///
    /// The execution block counts too. A quote taken at the last block before expiry executes in
    /// the next one, where the market reverts with `MarketExpired`.
    fn latest_known_time(&self) -> u64 {
        self.rate_sampled_at
            .max(self.head_timestamp)
            .max(self.execution_timestamp)
    }

    fn guard_current(&self) -> Result<(), SimulationError> {
        if !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle market rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// The most PT the market will absorb, from the 96% proportion cap.
    fn max_pt_in(&self) -> Result<U256, PendleError> {
        let comp = self.pre_compute()?;
        approx::calc_soft_max_pt_in(&self.market, &comp)
    }

    /// The most PT the router's search will take out, from the exchange-rate floor less its
    /// 999/1000 haircut.
    fn max_pt_out(&self) -> Result<U256, PendleError> {
        let comp = self.pre_compute()?;
        approx::calc_max_pt_out(&comp, self.market.total_pt)
    }

    /// The market at the execution block with a risen index, when `apply_block` has recorded one
    /// later than the sampled block; empty otherwise.
    ///
    /// This state already quotes at that block's time, with the index as sampled. The index may
    /// have risen by then: it accrues with the yield token, and nothing here projects it. It can
    /// also fall: the indexed index is `max(SY.exchangeRate(), pyIndexStored)` with the stored
    /// value followed from events, and the YT stores a new one only when someone interacts with
    /// it, so an exchange rate that dips between the two blocks lowers the index the execution
    /// block uses. That case is not covered here. The returned market carries the index
    /// [`INDEX_DRIFT`] above the sampled one. Whether a trade fills is not monotone in the index
    /// near a bound, so this and the sampled index alone do not cover the indices between them;
    /// [`EXECUTION_HAIRCUT`] keeps the limits clear of the bound.
    fn at_execution(&self) -> Vec<Self> {
        if self.execution_timestamp <= self.rate_sampled_at {
            return Vec::new();
        }
        // Rounded up, so it is never zero: an index below 1e12 wei would otherwise get no drift at
        // all, though it moves in whole wei.
        let drift = self
            .py_index
            .saturating_mul(INDEX_DRIFT)
            .div_ceil(ONE_E18);
        vec![Self { py_index: self.py_index.saturating_add(drift), ..self.clone() }]
    }

    /// The most PT the market itself will send out before its exchange rate falls below par.
    fn max_pt_out_unhaircut(&self) -> Result<U256, PendleError> {
        let comp = self.pre_compute()?;
        approx::calc_max_pt_out_unhaircut(&comp, self.market.total_pt)
    }

    /// `amount`, a bound whose trade takes `pt` PT to the exchange-rate floor, less the share of
    /// it that [`ANCHOR_NOISE`] can move: zero when the noise is all of `pt`.
    fn clear_of_anchor_noise(&self, amount: U256, pt: U256) -> Result<U256, PendleError> {
        let comp = self.pre_compute()?;
        let total_pt = pmath::to_u256(self.market.total_pt)?;
        let total_asset = pmath::to_u256(comp.total_asset)?;
        let reserves = total_pt.saturating_add(total_asset);
        if reserves.is_zero() {
            return Ok(U256::ZERO);
        }
        let noise = (total_pt.saturating_mul(total_asset) / reserves)
            .saturating_mul(pmath::to_u256(comp.rate_scalar)?)
            .saturating_mul(ANCHOR_NOISE)
            .div_ceil(ONE_E18 * ONE_E18);
        if noise >= pt {
            return Ok(U256::ZERO);
        }
        Ok(amount -
            amount
                .saturating_mul(noise)
                .div_ceil(pt))
    }
}

impl PendleSyState {
    /// Whether this wrapper still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    /// Refuses a quote this state cannot answer: any edge of a paused SY, and an `index_rate`
    /// edge whose rate the chain has moved past. A `one_to_one` edge is a decimal rescale that
    /// never reads the rate, so its clock does not matter.
    fn guard_quotable(&self, class: TokenClass) -> Result<(), SimulationError> {
        if self.paused {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY {} is paused",
                self.sy_address
            )));
        }
        if class == TokenClass::IndexRate && !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// The most SY a redemption to `token` can take.
    ///
    /// Supply bounds every exit: no more shares can be burned than exist. On a mainnet fork, 82 of
    /// the 88 exits of live SYs redeemed their whole supply to the wei. An exit to the SY's own
    /// yield token is also paid out of what the SY holds of it, so the payout cannot pass that
    /// holding; one live SY held 28 wei less than its supply needed. The holding counts only when
    /// it backs at least 999/1000 of the whole supply's payout: an SY that keeps its yield
    /// token in a staking contract holds little of it and pays from there.
    ///
    /// An exit through a protocol the SY wraps can be limited further, by that protocol's own
    /// liquidity or pause, which nothing here can see.
    fn max_redeem_sy(
        &self,
        token: &Bytes,
        class: TokenClass,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        let supply = self.total_supply;
        let (Some(yield_token), Some(held)) = (&self.yield_token, self.yield_token_held) else {
            return Ok(supply);
        };
        if token != yield_token || !self.holding_backs_supply {
            return Ok(supply);
        }
        let convert =
            |amount: U256| self.convert(amount, class, Direction::Redeem, token, token_decimals);
        largest_input_within(held, supply, convert)
    }

    /// Judges whether the SY pays a redemption to its yield token out of its holding: it does when
    /// the holding covers at least 999/1000 of what redeeming the whole supply pays. An SY that
    /// keeps its yield token in a staking contract holds little of it and pays from there.
    ///
    /// Judged on the stream's state only, at decode and on each delta. A quote burns supply and
    /// pays out of the holding, which leaves the shortfall as it was but makes it a larger share
    /// of what remains; judging again there would flip an SY that is a few wei short to "pays from
    /// elsewhere" and report more than it holds.
    pub(super) fn judge_holding(&mut self) {
        self.holding_backs_supply = (|| {
            let yield_token = self.yield_token.as_ref()?;
            let held = self.yield_token_held?;
            let class = *self.tokens_out.get(yield_token)?;
            let token_decimals = *self.token_decimals.get(yield_token)?;
            let payout = self
                .convert(self.total_supply, class, Direction::Redeem, yield_token, token_decimals)
                .ok()?;
            Some(held >= payout - payout / U256::from(1000))
        })()
        .unwrap_or(false);
    }

    /// The SY a deposit can still mint before the supply cap; `None` for an uncapped SY.
    fn deposit_headroom(&self) -> Option<U256> {
        self.supply_cap
            .map(|cap| cap.saturating_sub(self.total_supply))
    }

    /// The SY's balance of a component token, from the stream's component balances, keyed by the
    /// component id the indexer gives an SY: its address.
    fn apply_balances(&mut self, balances: &Balances) {
        let (Some(yield_token), Some(component)) = (
            &self.yield_token,
            balances
                .component_balances
                .get(&self.sy_address.to_string()),
        ) else {
            return;
        };
        if let Some(balance) = component.get(yield_token) {
            // A balance past 256 bits is malformed; the holding is then unknown and supply alone
            // bounds a redemption.
            self.yield_token_held = u256_from_be(balance, "yield token balance").ok();
        }
    }

    /// `sy` shares in yield-token units, at the yield token's redemption conversion; `None` when
    /// the yield token has no exit class or no known decimals.
    fn in_yield_token(&self, sy: U256) -> Option<U256> {
        let yield_token = self.yield_token.as_ref()?;
        let class = *self.tokens_out.get(yield_token)?;
        let decimals = *self.token_decimals.get(yield_token)?;
        self.convert(sy, class, Direction::Redeem, yield_token, decimals)
            .ok()
    }

    /// The same wrapper after a conversion: a deposit mints the SY it returns, a redemption burns
    /// the SY it takes.
    ///
    /// What it holds of its yield token moves too: a deposit of it adds to the holding, and a
    /// redemption to it pays out of it.
    fn after(
        &self,
        direction: Direction,
        token: &Bytes,
        amount_in: U256,
        amount_out: U256,
    ) -> Result<Self, PendleError> {
        let mut next = self.clone();
        // The SY holds its yield token, so every deposit and redemption moves that holding, not
        // only one in the yield token itself: depositing stETH into SY-wstETH wraps it, and
        // redeeming to stETH unwraps wstETH. Through another token, the move is the SY amount in
        // yield-token units; a holding that cannot be converted becomes unknown.
        next.yield_token_held = if self.yield_token.as_ref() == Some(token) {
            self.yield_token_held
                .map(|held| match direction {
                    Direction::Deposit => held.saturating_add(amount_in),
                    Direction::Redeem => held.saturating_sub(amount_out),
                })
        } else {
            let sy_moved = match direction {
                Direction::Deposit => amount_out,
                Direction::Redeem => amount_in,
            };
            self.yield_token_held
                .zip(self.in_yield_token(sy_moved))
                .map(|(held, moved)| match direction {
                    Direction::Deposit => held.saturating_add(moved),
                    Direction::Redeem => held.saturating_sub(moved),
                })
        };
        next.total_supply = match direction {
            Direction::Deposit => self
                .total_supply
                .checked_add(amount_out)
                .ok_or(PendleError::Overflow { operation: "SY supply after a deposit" })?,
            Direction::Redeem => self
                .total_supply
                .checked_sub(amount_in)
                .ok_or(PendleError::Overflow { operation: "SY supply after a redemption" })?,
        };
        Ok(next)
    }
}

fn biguint_to_f64(value: &BigUint) -> f64 {
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::INFINITY)
}

fn unknown_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not an edge of this Pendle component",
        token_in.address, token_out.address
    ))
}

fn unquotable_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not a quotable conversion for this SY: the indexer could not classify it, so \
         no closed form is known",
        token_in.address, token_out.address
    ))
}

/// The soft limit this repo uses for an edge with no reserve behind it, matching
/// `native_wrapper`. Large enough not to constrain any real trade, small enough that a caller can
/// multiply by it without overflowing.
///
/// An uncapped SY's deposit reports it. Depositing the SY's own yield token has no limit at all:
/// on a mainnet fork, 51 of 53 such deposits took a trillion whole tokens. A deposit converted
/// through the protocol the SY wraps can be limited there (17 of 47 were, by an ERC-4626
/// `maxDeposit`, Lido's stake limit and others), which nothing here can see.
fn unbounded_soft_limit() -> U256 {
    U256::from(u128::MAX)
}

/// The SY-in limit (SY→PT, SY→YT) from `exact`, the input that costs the search's hard bound.
///
/// Without an execution block, the limit is exact: the largest input the search accepts, `eps`
/// slack included. With one, it is the bound less [`EXECUTION_HAIRCUT`], and reports no depth if
/// that is under [`SY_IN_DUST`] or does not fill at every checked state.
fn sy_in_limit(
    exact: U256,
    for_execution: bool,
    fills: impl Fn(U256) -> bool,
) -> Result<U256, PendleError> {
    if !for_execution {
        return Ok(largest_fill(approx::with_eps_slack(exact)?, exact, fills));
    }
    let limit = haircut(exact);
    Ok(if limit >= SY_IN_DUST && fills(limit) { limit } else { U256::ZERO })
}

/// The least SY, in wei, an SY-in limit must take to hold at the execution block. The search
/// accepts a guess whose cost lands within `eps` (5e-5) below the request, and each guess's cost
/// rounds by a few wei of SY, which shift with the index. On a request whose window spans only a
/// few wei, the search lands or runs out of iterations depending on the index: probes found such
/// failures up to a window of 4 wei. Two million wei gives a window of 100. Only an SY-in edge
/// whose whole depth is dust has less, as near the proportion cap or in the last hour to expiry.
/// Where an asset wei is worth many SY wei (an index far below 1e18), costs move in steps that
/// size, and a larger limit can miss them too: it then fails its fill check and reports no depth.
const SY_IN_DUST: U256 = U256::from_limbs([2_000_000, 0, 0, 0]);

/// The least SY a YT sale must pay at the execution block, in wei, for its limit to hold there.
/// Selling YT repays borrowed PT out of the PY the sale frees, through conversions that each round
/// by a wei of SY, so a sale paying a few wei fills or not depending on the index. In any SY, a
/// thousand wei is dust. The repayment also rounds by a wei of PY, which at an index far below
/// 1e18 is worth many SY wei; a sale paying less than that fails its fill check instead.
const YT_SALE_DUST: U256 = U256::from_limbs([1_000, 0, 0, 0]);

/// `bound` less [`EXECUTION_HAIRCUT`] of it.
fn haircut(bound: U256) -> U256 {
    bound -
        bound
            .saturating_mul(EXECUTION_HAIRCUT)
            .div_ceil(ONE_E18)
}

/// The largest input a market edge fills, found from `candidate`, a closed form for the edge's
/// boundary.
///
/// The closed forms land within a few wei of the boundary, except where the contract's own
/// estimate rounds (selling YT, to about 1e-11 of the amount). If the edge fills at `candidate`, a
/// few wei above it are tried; if it does not, it is walked down in doubling steps until it does,
/// which lands within twice the miss. `fallback`, a bound the edge is known to fill, covers a
/// candidate that misses by more than a thousandth, which no tested state does.
fn largest_fill(candidate: U256, fallback: U256, fills: impl Fn(U256) -> bool) -> U256 {
    if fills(candidate) {
        let mut largest = candidate;
        for _ in 0..4 {
            let next = largest + U256::from(1);
            if !fills(next) {
                break;
            }
            largest = next;
        }
        return largest;
    }
    let mut step = U256::from(1);
    while step <= candidate / U256::from(1000) {
        if fills(candidate - step) {
            return candidate - step;
        }
        step <<= 1;
    }
    fallback
}

/// The largest input in `[0, upper]` that fills, by bisection, taking filling to stop at one
/// boundary. Zero when nothing above zero fills.
fn largest_filling_input(upper: U256, fills: impl Fn(U256) -> bool) -> U256 {
    let (mut filling, mut failing) = (U256::ZERO, upper.saturating_add(U256::from(1)));
    while failing - filling > U256::from(1) {
        let middle = filling + (failing - filling) / U256::from(2);
        if fills(middle) {
            filling = middle;
        } else {
            failing = middle;
        }
    }
    filling
}

/// The largest input in `[0, upper]` whose conversion stays at or under `target`.
///
/// A wrap conversion is monotonic and plain integer arithmetic, so a bisection over it costs a few
/// hundred multiplications.
fn largest_input_within(
    target: U256,
    upper: U256,
    convert: impl Fn(U256) -> Result<U256, PendleError>,
) -> Result<U256, PendleError> {
    if convert(upper)? <= target {
        return Ok(upper);
    }
    let (mut within, mut past) = (U256::ZERO, upper);
    while past - within > U256::from(1) {
        let middle = within + (past - within) / U256::from(2);
        if convert(middle)? <= target {
            within = middle;
        } else {
            past = middle;
        }
    }
    Ok(within)
}

/// The input in `[0, upper]` that yields the most output, for an edge whose output rises with the
/// input and then falls, as selling YT does once buying back the borrowed PT costs more than the
/// PY it frees.
///
/// A golden-section search over the quote, which reuses one evaluation per step, stopped once the
/// range is a hundred-thousandth of `upper`: the output is flat at its peak, so a closer input
/// changes it by far less. An input that does not quote counts as no output. Returns the best
/// input it evaluated, so the result always quotes, or zero when nothing in the range yields
/// anything.
fn most_output_within(upper: U256, output: impl Fn(U256) -> Option<U256>) -> U256 {
    // 1 / golden ratio, to six places.
    let section =
        |low: U256, high: U256| (high - low) * U256::from(618_034u32) / U256::from(1_000_000u32);
    let mut best = (U256::ZERO, U256::ZERO);
    let mut evaluate = |amount: U256| {
        let out = output(amount).unwrap_or(U256::ZERO);
        if out > best.1 {
            best = (amount, out);
        }
        out
    };
    evaluate(upper);
    let tolerance = (upper / U256::from(100_000u32)).max(U256::from(3u8));
    let (mut low, mut high) = (U256::ZERO, upper);
    let (mut left, mut right) = (high - section(low, high), low + section(low, high));
    let (mut left_out, mut right_out) = (evaluate(left), evaluate(right));
    while high - low > tolerance && left < right {
        if left_out < right_out {
            low = left;
            left = right;
            left_out = right_out;
            right = low + section(low, high);
            right_out = evaluate(right);
        } else {
            high = right;
            right = left;
            right_out = left_out;
            left = high - section(low, high);
            left_out = evaluate(left);
        }
    }
    best.0
}

/// A market edge's limits, as [`ProtocolSim::get_limits`] documents them, computed afresh.
fn market_limits(
    state: &PendleMarketState,
    sell_token: Bytes,
    buy_token: Bytes,
) -> Result<(BigUint, BigUint), SimulationError> {
    if state.expired() {
        // Dead, not merely empty. Zero depth in both directions.
        return Ok(no_depth());
    }
    // Every edge moves SY, which a paused SY refuses to transfer, and no edge can be
    // priced without the fee.
    if state.sy_paused || state.fee_unread {
        return Ok(no_depth());
    }
    // A bound this state cannot quote against is not a bound. Reported as no depth
    // rather than an error, so a router skips the market the way it skips an expired
    // one instead of treating a routine gap as a failure.
    if !state.is_current() {
        return Ok(no_depth());
    }
    // No liquidity yet. Every bound below divides by the reserves.
    if state.market.total_pt <= I256::ZERO || state.market.total_sy <= I256::ZERO {
        return Ok(no_depth());
    }
    let (Some(from), Some(to)) = (state.role(&sell_token), state.role(&buy_token)) else {
        return Ok(no_depth());
    };

    // Each limit starts from a closed form for the edge's boundary, checked against
    // the quote, which replicates the router to the wei. Without an execution block
    // the limit is that boundary, exact. With one (`apply_block`), the state quotes at
    // that block's time, but the index may have risen by then. Each closed form then
    // starts from the smaller of its values with the index as sampled and
    // `INDEX_DRIFT` above it, gives up `EXECUTION_HAIRCUT`, and must still fill at
    // both.
    let at_execution = state.at_execution();
    let states: Vec<&PendleMarketState> = std::iter::once(state)
        .chain(&at_execution)
        .collect();
    let fills = |amount: U256| {
        states.iter().all(|state| {
            state
                .amount_out(amount, from, to)
                .is_ok()
        })
    };
    // A bound a checked state cannot evaluate counts as zero: no depth. That is a rate
    // floor the clock has carried the market past, or a market whose closed form leaves
    // the math's domain, such as an exponent past -41 on a steep curve or a PT reserve
    // of dust; the router's search, which evaluates the same form, reverts there too.
    let smallest = |bound: &dyn Fn(&PendleMarketState) -> Result<U256, PendleError>| {
        states
            .iter()
            .map(|state| bound(state).unwrap_or(U256::ZERO))
            .min()
            .unwrap_or(U256::ZERO)
    };
    let for_execution = !at_execution.is_empty();
    let max_in = match (from, to) {
        // The router's search stops at the haircut rate floor, and without an execution
        // block accepts a request up to `eps` above what that bound costs.
        (MarketToken::Sy, MarketToken::Pt) => {
            let exact = smallest(&|state| {
                let (sy_in, pt_out) = approx::max_sy_in_for_pt_with(
                    &state.market,
                    &state.pre_compute()?,
                    state.py_index,
                )?;
                if for_execution {
                    state.clear_of_anchor_noise(sy_in, pt_out)
                } else {
                    Ok(sy_in)
                }
            });
            sy_in_limit(exact, for_execution, fills)?
        }
        // The same search, bounded by the proportion cap the minted PT is sold into.
        (MarketToken::Sy, MarketToken::Yt) => {
            let exact = smallest(&|state| {
                approx::max_sy_in_for_yt_with(&state.market, &state.pre_compute()?, state.py_index)
                    .map(|(sy_in, _)| sy_in)
            });
            sy_in_limit(exact, for_execution, fills)?
        }
        // Selling PT hands PT *to* the market, so the 96% proportion cap binds, and the
        // market checks it directly: the closed form is the boundary.
        (MarketToken::Pt, MarketToken::Sy) => {
            let bound = smallest(&|state| state.max_pt_in());
            let bound = if for_execution { haircut(bound) } else { bound };
            if fills(bound) {
                bound
            } else {
                U256::ZERO
            }
        }
        // Selling YT flash-*borrows* PT from the market and buys it back, fee included,
        // so each extra YT pays less, and past some size it pays nothing or costs SY:
        // the trader's output peaks well before the market's rate floor stops the
        // trade. The limit is that peak, searched for within what fills; a larger
        // sale executes but returns less.
        (MarketToken::Yt, MarketToken::Sy) => {
            let candidate = smallest(&|state| {
                let bound = state.max_pt_out_unhaircut()?;
                if for_execution {
                    state.clear_of_anchor_noise(bound, bound)
                } else {
                    Ok(bound)
                }
            });
            // The sale itself never evaluates that closed form, so where it cannot be
            // evaluated the sale may still fill: the boundary is searched for instead.
            let candidate = if candidate.is_zero() {
                largest_filling_input(pmath::to_u256(state.market.total_pt)?, fills)
            } else {
                candidate
            };
            let fillable = largest_fill(candidate, smallest(&|state| state.max_pt_out()), fills);
            // Where the output still rises at the bound, the peak lands on it, so the
            // search keeps to the haircut bound.
            let fillable = if for_execution { haircut(fillable) } else { fillable };
            let output = |amount: U256| {
                state
                    .amount_out(amount, from, to)
                    .ok()
                    .map(|swap| swap.amount_out)
            };
            // Within `fillable` the sale fills at every checked state, so the search
            // follows the sampled state's output alone and checks only its answer.
            let peak = most_output_within(fillable, output);
            if fills(peak) {
                peak
            } else {
                U256::ZERO
            }
        }
        // PT against YT, or either against itself: not an edge of this market.
        _ => return Ok(no_depth()),
    };
    // `get_amount_out` refuses any amount above `u128::MAX`, so a limit past it would
    // not be quotable; it is clamped there, where it must still fill.
    let max_in = if max_in > unbounded_soft_limit() {
        let clamped = unbounded_soft_limit();
        if fills(clamped) {
            clamped
        } else {
            U256::ZERO
        }
    } else {
        max_in
    };
    if max_in.is_zero() {
        return Ok(no_depth());
    }
    let max_out = state
        .amount_out(max_in, from, to)?
        .amount_out;
    // An input that fills but yields nothing is no depth, not a trade. A YT sale that
    // must hold at the execution block also needs to pay more than the repayment's
    // rounding there, which comes in whole wei of SY: below that, a sale that fills at
    // the checked states can fail at an index between them. Only the execution block
    // matters, so without one there is no such check: a sale paying a thousand wei an
    // hour before expiry pays none a second before it.
    let dust = (from, to) == (MarketToken::Yt, MarketToken::Sy) &&
        for_execution &&
        states.iter().any(|state| {
            state
                .amount_out(max_in, from, to)
                .map_or(true, |swap| swap.amount_out < YT_SALE_DUST)
        });
    if max_out.is_zero() || dust {
        return Ok(no_depth());
    }
    Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
}

/// What `get_limits` reports for a pair this component cannot trade.
fn no_depth() -> (BigUint, BigUint) {
    (BigUint::from(0u32), BigUint::from(0u32))
}

/// Reads an attribute as a big-endian unsigned integer.
pub(super) fn attribute_u256(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<U256>, ValueTooWide> {
    attributes
        .get(name)
        .map(|raw| u256_from_be(raw, name))
        .transpose()
}

/// A big-endian value that does not fit in 256 bits.
#[derive(Debug)]
pub(super) struct ValueTooWide {
    name: String,
    len: usize,
}

impl fmt::Display for ValueTooWide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is {} bytes, more than a 256-bit value holds", self.name, self.len)
    }
}

impl From<ValueTooWide> for TransitionError {
    fn from(error: ValueTooWide) -> Self {
        TransitionError::DecodeError(error.to_string())
    }
}

/// Reads a market reserve, which is never negative.
///
/// Read unsigned and then narrowed, so it decodes whether the indexer wrote signed bytes or the
/// unsigned magnitude its balance aggregation produces. A signed read takes a magnitude whose top
/// bit is set as negative: 10e18 is `0x8ac7…`. A reserve of 2^255 or more cannot be one.
pub(super) fn attribute_reserve(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<I256>, ValueTooWide> {
    let Some(raw) = attributes.get(name) else { return Ok(None) };
    let value = u256_from_be(raw, name)?;
    I256::try_from(value)
        .map(Some)
        .map_err(|_| ValueTooWide { name: name.to_string(), len: raw.len() })
}

/// Reads a big-endian unsigned value, which may carry leading zero bytes past 32: the indexer
/// writes unsigned values with `to_signed_bytes_be`, so anything at or above 2^255 arrives as 33
/// bytes with a leading zero.
pub(super) fn u256_from_be(raw: &[u8], name: &str) -> Result<U256, ValueTooWide> {
    let significant = &raw[raw
        .iter()
        .take_while(|byte| **byte == 0)
        .count()..];
    if significant.len() > 32 {
        return Err(ValueTooWide { name: name.to_string(), len: raw.len() });
    }
    Ok(U256::from_be_slice(significant))
}

/// Reads an attribute as a big-endian signed integer, `None` when it is absent.
///
/// The indexer writes minimal two's complement, which fits any 256-bit value in 32 bytes, so a
/// wider value is an error rather than something to truncate.
pub(super) fn attribute_i256(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<I256>, ValueTooWide> {
    let Some(raw) = attributes.get(name) else { return Ok(None) };
    if raw.len() > 32 {
        return Err(ValueTooWide { name: name.to_string(), len: raw.len() });
    }
    let mut padded = [0u8; 32];
    let start = 32 - raw.len();
    padded[start..].copy_from_slice(raw);
    // Sign-extend, since the indexer writes minimal two's complement.
    if raw
        .first()
        .is_some_and(|b| b & 0x80 != 0)
    {
        for byte in padded.iter_mut().take(start) {
            *byte = 0xff;
        }
    }
    Ok(Some(I256::from_be_bytes(padded)))
}

impl PendleMarketState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attribute_reserve(attributes, "total_pt")? {
            self.market.total_pt = value;
        }
        if let Some(value) = attribute_reserve(attributes, "total_sy")? {
            self.market.total_sy = value;
        }
        if let Some(value) = attribute_u256(attributes, "last_ln_implied_rate")? {
            self.market.last_ln_implied_rate = value;
        }
        if let Some(value) = attribute_u256(attributes, "ln_fee_rate_root")? {
            self.market.ln_fee_rate_root = value;
            // The indexer emits the two fee attributes together.
            self.fee_unread = false;
        }
        if let Some(value) = attribute_u256(attributes, "reserve_fee_percent")? {
            self.market.reserve_fee_percent = value;
        }
        // A zero index cannot price anything: it reads as never sampled, as in the decoder, and
        // the timestamp beside it is not taken.
        let index = attribute_u256(attributes, "py_index_current")?;
        if let Some(value) = index {
            if value.is_zero() {
                self.rate_sampled_at = clock::NEVER_SAMPLED;
            } else {
                self.py_index = value;
            }
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at")? {
            if !index.is_some_and(|index| index.is_zero()) {
                self.rate_sampled_at = value.saturating_to();
            }
        }
        if let Some(value) = attribute_u256(attributes, SY_PAUSED)? {
            self.sy_paused = !value.is_zero();
        }
        // The head. The Substreams refresh emits it on every refresh block whether or not the rate
        // resolved, and the stream decoder also stamps it on every delta it passes on. So with
        // `sy_rate_refresh_blocks` above 1, a market a block touches without a refresh has its
        // head move past `rate_sampled_at` and refuses until the next refresh, while a market no
        // delta touches keeps its old head and goes on quoting the sampled curve.
        if let Some(value) = attribute_u256(attributes, "block_timestamp")? {
            self.head_timestamp = value.saturating_to();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

impl PendleSyState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        // A drained or deprecated SY can report a zero rate. Every conversion divides by it, so it
        // reads as never sampled, as in the decoder: the `index_rate` edges refuse and report no
        // depth, the `one_to_one` edges need no rate and go on quoting, and nothing fails the
        // stream. The timestamp beside it is not taken.
        let rate = attribute_u256(attributes, "sy_exchange_rate")?;
        if let Some(value) = rate {
            if value.is_zero() {
                self.rate_sampled_at = clock::NEVER_SAMPLED;
            } else {
                self.exchange_rate = value;
            }
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at")? {
            if !rate.is_some_and(|rate| rate.is_zero()) {
                self.rate_sampled_at = value.saturating_to();
            }
        }
        if let Some(value) = attribute_u256(attributes, "block_timestamp")? {
            self.head_timestamp = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, SY_TOTAL_SUPPLY)? {
            self.total_supply = value;
        }
        if let Some(value) = attribute_u256(attributes, PAUSED)? {
            self.paused = !value.is_zero();
        }
        if let Some(value) = attribute_u256(attributes, SY_SUPPLY_CAP)? {
            self.supply_cap = lifted_cap_as_none(value);
        }
        if let Some(value) = attribute_u256(attributes, ENTRY_FEE_BPS)? {
            self.entry_fee_bps = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, ENTRY_FEE_FROM)? {
            self.entry_fee_from = value.saturating_to();
        }
        for (name, value) in attributes {
            let Some(token) = step_attribute_token(name).map_err(TransitionError::DecodeError)?
            else {
                continue;
            };
            let step = u256_from_be(value, name)?;
            self.deposit_steps.insert(token, step);
        }
        if delta
            .deleted_attributes
            .contains(SY_SUPPLY_CAP)
        {
            self.supply_cap = None;
        }
        // An upgrade re-classifies the SY's tokens: each class arrives as an update, and a token
        // that no longer matches either closed form arrives as `NO_CLASS`.
        for (name, value) in attributes {
            let Some((direction, token)) =
                class_attribute_token(name).map_err(TransitionError::DecodeError)?
            else {
                continue;
            };
            if value.as_ref() == NO_CLASS {
                self.classes_mut(direction)
                    .remove(&token);
                continue;
            }
            let class = TokenClass::parse(value).ok_or_else(|| {
                TransitionError::DecodeError(format!(
                    "{name} carries an unknown token class {:?}",
                    String::from_utf8_lossy(value)
                ))
            })?;
            self.classes_mut(direction)
                .insert(token, class);
        }
        for name in &delta.deleted_attributes {
            let Some((direction, token)) =
                class_attribute_token(name).map_err(TransitionError::DecodeError)?
            else {
                continue;
            };
            self.classes_mut(direction)
                .remove(&token);
        }
        Ok(())
    }

    fn classes_mut(&mut self, direction: Direction) -> &mut HashMap<Bytes, TokenClass> {
        match direction {
            Direction::Deposit => &mut self.tokens_in,
            Direction::Redeem => &mut self.tokens_out,
        }
    }
}

/// State attribute: SY shares outstanding.
pub(super) const SY_TOTAL_SUPPLY: &str = "sy_total_supply";
/// State attribute: whether the SY is paused.
pub(super) const PAUSED: &str = "paused";
/// State attribute on a market: whether its SY is paused.
pub(super) const SY_PAUSED: &str = "sy_paused";
/// State attribute: the cap on an SY's own supply, absent when it has none and
/// `type(uint256).max` once lifted.
pub(super) const SY_SUPPLY_CAP: &str = "sy_supply_cap";
/// The class the indexer gives a token and direction that matches neither closed form after an
/// upgrade, in place of deleting the class: the pair is unquotable.
pub(super) const NO_CLASS: &[u8] = b"none";

/// The supply cap an `SY_SUPPLY_CAP` value sets: `None` for `type(uint256).max`, the value an SY
/// reports once its cap is lifted.
pub(super) fn lifted_cap_as_none(cap: U256) -> Option<U256> {
    (cap != U256::MAX).then_some(cap)
}
/// Static attribute: the token an SY holds as its backing.
pub(super) const YIELD_TOKEN: &str = "yield_token";
/// Prefixes of the per-token class attributes, followed by the token's hex address.
pub(super) const TOKEN_IN_CLASS_PREFIX: &str = "token_in_class_0x";
pub(super) const TOKEN_OUT_CLASS_PREFIX: &str = "token_out_class_0x";
/// Prefix of the per-token deposit step, followed by the token's hex address.
pub(super) const TOKEN_IN_STEP_PREFIX: &str = "token_in_step_0x";
/// State attributes: the vault's entry fee in basis points, and the timestamp it applies from.
pub(super) const ENTRY_FEE_BPS: &str = "entry_fee_bps";
pub(super) const ENTRY_FEE_FROM: &str = "entry_fee_from";

/// The token a deposit-step attribute names, or `None` for any other attribute.
pub(super) fn step_attribute_token(name: &str) -> Result<Option<Bytes>, String> {
    let Some(suffix) = name.strip_prefix(TOKEN_IN_STEP_PREFIX) else { return Ok(None) };
    let token = Bytes::from_str(&format!("0x{suffix}"))
        .map_err(|e| format!("{name} does not name an address: {e}"))?;
    if token.len() != 20 {
        return Err(format!("{name} names {} bytes, not a 20-byte address", token.len()));
    }
    Ok(Some(token))
}

/// The direction and token a class attribute names, or `None` for any other attribute.
///
/// A class attribute whose suffix is not a 20-byte address is an error, the same for a snapshot
/// and a delta, rather than an attribute to skip: skipping it would leave a class the indexer
/// meant to set or delete unapplied.
pub(super) fn class_attribute_token(name: &str) -> Result<Option<(Direction, Bytes)>, String> {
    let (direction, suffix) = if let Some(suffix) = name.strip_prefix(TOKEN_IN_CLASS_PREFIX) {
        (Direction::Deposit, suffix)
    } else if let Some(suffix) = name.strip_prefix(TOKEN_OUT_CLASS_PREFIX) {
        (Direction::Redeem, suffix)
    } else {
        return Ok(None);
    };
    let token = Bytes::from_str(&format!("0x{suffix}"))
        .map_err(|e| format!("{name} does not name an address: {e}"))?;
    if token.len() != 20 {
        return Err(format!("{name} names {} bytes, not a 20-byte address", token.len()));
    }
    Ok(Some((direction, token)))
}
