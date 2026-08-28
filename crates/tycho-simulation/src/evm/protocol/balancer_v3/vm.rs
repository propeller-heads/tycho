//! Reads Balancer V3 pool state from the locally indexed VM storage via view-getter calls and
//! assembles the [`PoolState`] that `balancer_maths_rust` quotes against.
//!
//! Every value the maths needs is exposed by a getter, so nothing has to be indexed on top of what
//! `vm:balancer_v3` already tracks: `get<Type>PoolDynamicData` covers the live balances, token
//! rates, swap fee, total supply and amplification, `get<Type>PoolImmutableData` the scaling
//! factors and weights, and the Vault's `getPoolConfig` the aggregate swap fee. Values that come
//! from other protocols — rate providers above all — are computed by executing their code against
//! the storage the DCI already keeps fresh, exactly as the VM adapter does today.
use std::{collections::HashMap, fmt::Debug};

use alloy::{
    core::sol,
    primitives::{address, Address as AlloyAddress, U256},
    sol_types::SolCall,
};
use balancer_maths_rust::{
    common::types::{BasePoolState, PoolState},
    pools::{
        quantamm::quantamm_data::{QuantAmmImmutable, QuantAmmMutable, QuantAmmState},
        reclammv2::reclammv2_data::{ReClammV2Immutable, ReClammV2Mutable, ReClammV2State},
        stable::stable_data::{StableMutable, StableState},
        weighted::WeightedState,
    },
};
use revm::DatabaseRef;
use serde::{Deserialize, Serialize};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::{u256_num::biguint_to_u256, vm::utils::json_deserialize_be_bigint_list},
    simulation::{SimulationEngine, SimulationParameters},
};

sol! {
    #[allow(missing_docs)]
    struct LiquidityManagement {
        bool disableUnbalancedLiquidity;
        bool enableAddLiquidityCustom;
        bool enableRemoveLiquidityCustom;
        bool enableDonation;
    }

    #[allow(missing_docs)]
    struct PoolConfig {
        LiquidityManagement liquidityManagement;
        uint256 staticSwapFeePercentage;
        uint256 aggregateSwapFeePercentage;
        uint256 aggregateYieldFeePercentage;
        uint40 tokenDecimalDiffs;
        uint32 pauseWindowEndTime;
        bool isPoolRegistered;
        bool isPoolInitialized;
        bool isPoolPaused;
        bool isPoolInRecoveryMode;
    }

    #[allow(missing_docs)]
    struct WeightedPoolDynamicData {
        uint256[] balancesLiveScaled18;
        uint256[] tokenRates;
        uint256 staticSwapFeePercentage;
        uint256 totalSupply;
        bool isPoolInitialized;
        bool isPoolPaused;
        bool isPoolInRecoveryMode;
    }

    #[allow(missing_docs)]
    struct StablePoolDynamicData {
        uint256[] balancesLiveScaled18;
        uint256[] tokenRates;
        uint256 staticSwapFeePercentage;
        uint256 totalSupply;
        uint256 bptRate;
        uint256 amplificationParameter;
        uint256 startValue;
        uint256 endValue;
        uint32 startTime;
        uint32 endTime;
        bool isAmpUpdating;
        bool isPoolInitialized;
        bool isPoolPaused;
        bool isPoolInRecoveryMode;
    }

    #[allow(missing_docs)]
    struct ReClammPoolDynamicData {
        uint256[] balancesLiveScaled18;
        uint256[] tokenRates;
        uint256 staticSwapFeePercentage;
        uint256 totalSupply;
        uint256 lastTimestamp;
        uint256[] lastVirtualBalances;
        uint256 dailyPriceShiftExponent;
        uint256 dailyPriceShiftBase;
        uint256 centerednessMargin;
        uint256 currentPriceRatio;
        uint256 currentFourthRootPriceRatio;
        uint256 startFourthRootPriceRatio;
        uint256 endFourthRootPriceRatio;
        uint32 priceRatioUpdateStartTime;
        uint32 priceRatioUpdateEndTime;
        bool isPoolInitialized;
        bool isPoolPaused;
        bool isPoolInRecoveryMode;
    }

    #[allow(missing_docs)]
    struct QuantAmmPoolDynamicData {
        uint256[] balancesLiveScaled18;
        uint256[] tokenRates;
        uint256 totalSupply;
        bool isPoolInitialized;
        bool isPoolPaused;
        bool isPoolInRecoveryMode;
        int256[] firstFourWeightsAndMultipliers;
        int256[] secondFourWeightsAndMultipliers;
        uint40 lastUpdateTime;
        uint40 lastInteropTime;
    }

    #[allow(missing_docs)]
    struct QuantAmmPoolImmutableData {
        address[] tokens;
        uint256 oracleStalenessThreshold;
        uint256 poolRegistry;
        int256[][] ruleParameters;
        uint64[] lambda;
        uint64 epsilonMax;
        uint64 absoluteWeightGuardRail;
        uint64 updateInterval;
        uint256 maxTradeSizeRatio;
    }

    #[allow(missing_docs)]
    interface IBalancerV3Pool {
        function getWeightedPoolDynamicData() external view returns (WeightedPoolDynamicData memory);
        function getMinTokenBalances() external view returns (uint256[] memory);
        function getStablePoolDynamicData() external view returns (StablePoolDynamicData memory);
        function getReClammPoolDynamicData() external view returns (ReClammPoolDynamicData memory);
        function getQuantAMMWeightedPoolDynamicData() external view returns (QuantAmmPoolDynamicData memory);
        function getQuantAMMWeightedPoolImmutableData() external view returns (QuantAmmPoolImmutableData memory);
    }

    #[allow(missing_docs)]
    struct HooksConfig {
        bool enableHookAdjustedAmounts;
        bool shouldCallBeforeInitialize;
        bool shouldCallAfterInitialize;
        bool shouldCallComputeDynamicSwapFee;
        bool shouldCallBeforeSwap;
        bool shouldCallAfterSwap;
        bool shouldCallBeforeAddLiquidity;
        bool shouldCallAfterAddLiquidity;
        bool shouldCallBeforeRemoveLiquidity;
        bool shouldCallAfterRemoveLiquidity;
        address hooksContract;
    }

    #[allow(missing_docs)]
    interface IBalancerV3Vault {
        function getPoolConfig(address pool) external view returns (PoolConfig memory);
        function getHooksConfig(address pool) external view returns (HooksConfig memory);
        function getPoolPausedState(address pool) external view returns (bool, uint32, uint32, address);
    }
}

impl HooksConfig {
    /// Whether a hook takes part in swapping, which the hookless maths here cannot reproduce.
    ///
    /// Liquidity hooks are ignored: they cannot change a swap's outcome. A dynamic-fee hook is
    /// enough on its own — quoting such a pool with its static fee produces an amount the Vault
    /// then refuses with `DynamicSwapFeeHookFailed`.
    fn affects_swaps(&self) -> bool {
        self.enableHookAdjustedAmounts ||
            self.shouldCallComputeDynamicSwapFee ||
            self.shouldCallBeforeSwap ||
            self.shouldCallAfterSwap
    }
}

/// Bit width `registerPool` packs each token's `18 - decimals` into inside `tokenDecimalDiffs`
/// (`PoolConfigConst.DECIMAL_DIFF_BITLENGTH`).
const DECIMAL_DIFF_BITLENGTH: usize = 5;
const DECIMAL_DIFF_MASK: u64 = (1 << DECIMAL_DIFF_BITLENGTH) - 1;

// Static attributes the `ethereum-balancer-v3` Substreams package emits.
const NORMALIZED_WEIGHTS_ATTRIBUTE: &str = "normalized_weights";
const POOL_TYPE_ATTRIBUTE: &str = "pool_type";

/// The Balancer V3 Vault, which every pool of every generation is registered with.
///
/// Balancer deploys it from the same CREATE2 salt on every chain, so the address is identical on
/// all four this package indexes — checked against the deployment manifests. A chain that ever
/// broke that would have no code here, and its pools would be rejected at decode time rather than
/// quoted against the wrong contract.
const VAULT: AlloyAddress = address!("bA1333333333a1BA1108E8412f11850A5C319bA9");

/// The pool families this module can quote natively. Balancer V3 exposes more (Gyro, LBP, and
/// anything built on the hook system); those are rejected at decode time rather than approximated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BalancerPoolType {
    Weighted,
    Stable,
    /// AutoRange pools, whose price range shifts with time.
    Reclamm,
    /// Weighted pools whose weights are interpolated by an off-chain rule engine.
    QuantAmm,
}

impl BalancerPoolType {
    /// Kept next to [`Self::factory_marker`] so adding a variant means touching both.
    const ALL: [Self; 4] = [Self::Weighted, Self::Stable, Self::Reclamm, Self::QuantAmm];

    fn from_factory_marker(marker: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.factory_marker() == marker)
    }

    /// The `pool_type` attribute value the Substreams package writes for this family.
    fn factory_marker(self) -> &'static str {
        match self {
            Self::Weighted => "WeightedPoolFactory",
            Self::Stable => "StablePoolFactory",
            Self::Reclamm => "ReClammPoolFactory",
            Self::QuantAmm => "QuantAMMWeightedPoolFactory",
        }
    }

    /// The `pool_type` string `balancer_maths_rust` keys its pool implementations on.
    ///
    /// One marker per family: Balancer keeps the swap maths stable across factory generations. The
    /// V3 reCLAMM pools share their maths with the library's V2 implementation, which Balancer
    /// confirmed, so both map onto `RECLAMM_V2`.
    fn maths_marker(self) -> &'static str {
        match self {
            Self::Weighted => "WEIGHTED",
            Self::Stable => "STABLE",
            Self::Reclamm => "RECLAMM_V2",
            Self::QuantAmm => "QUANT_AMM_WEIGHTED",
        }
    }
}

/// Parses a [`POOL_TYPE_ATTRIBUTE`] value into the family whose maths prices the pool.
///
/// The attribute names the factory family alone, without the generation that built the pool:
/// a family quotes the same across every generation this decoder sees, and where a generation
/// does differ the pool is probed for the feature — the weighted minimum balance is read from the
/// pool, and reCLAMM's earlier maths is kept out by indexing only the newest generation. Returns
/// the reason the value could not be used, for the caller to attribute to a pool.
pub(super) fn parse_pool_type(marker: &str) -> Result<BalancerPoolType, String> {
    BalancerPoolType::from_factory_marker(marker)
        .ok_or_else(|| format!("unsupported pool_type `{marker}`"))
}

/// Determines which pool family `pool` belongs to.
///
/// Reads the `pool_type` static attribute the Substreams package writes. Returns an error for any
/// family this module cannot quote, which keeps such pools out of the native decoder instead of
/// letting them be priced by the wrong maths.
pub(super) fn resolve_pool_type(
    static_attributes: &HashMap<String, Bytes>,
    pool: &AlloyAddress,
) -> Result<BalancerPoolType, SimulationError> {
    let raw = static_attributes
        .get(POOL_TYPE_ATTRIBUTE)
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "balancer_v3 pool {pool} has no `{POOL_TYPE_ATTRIBUTE}`"
            ))
        })?;
    let marker = String::from_utf8(raw.to_vec()).map_err(|e| {
        SimulationError::FatalError(format!("balancer_v3 pool_type is not UTF-8: {e}"))
    })?;
    parse_pool_type(&marker)
        .map_err(|reason| SimulationError::FatalError(format!("balancer_v3 pool {pool}: {reason}")))
}

/// Builds the maths library's pool state for `pool` from the indexed storage.
///
/// The returned state is a snapshot for the block the engine's database is at. This full read —
/// including the hook check and the immutable data — runs once per pool at decode time; updates
/// go through [`refresh_pool_state`], which re-reads only what storage can change.
/// `block_timestamp` is the timestamp quotes should be evaluated at — reCLAMM shifts its price
/// range with time, so it needs the current block's timestamp rather than the pool's last-updated
/// one.
pub(super) fn read_pool_state<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    pool_type: BalancerPoolType,
    tokens: &[Bytes],
    static_attributes: &HashMap<String, Bytes>,
    block_timestamp: u64,
) -> Result<PoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let config = read_pool_config(engine, pool)?;

    // The factory a pool came from says nothing about its hooks: a StablePoolFactory pool can carry
    // a dynamic-fee hook, and quoting it as hookless yields amounts the Vault rejects.
    let hooks: HooksConfig =
        call(engine, &VAULT, IBalancerV3Vault::getHooksConfigCall { pool: *pool })?;
    if hooks.affects_swaps() {
        return Err(SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} uses swap hook {:?}, which the native maths does not model",
            hooks.hooksContract
        )));
    }

    // Registration fixes the token order and each token's decimals, so both come from what the
    // indexer recorded and from the config already read, rather than from a further getter call.
    let tokens = tokens
        .iter()
        .map(|token| format!("0x{}", hex::encode(token)))
        .collect::<Vec<_>>();
    let scaling_factors = scaling_factors(&config, tokens.len());

    match pool_type {
        BalancerPoolType::Weighted => {
            let dynamic: WeightedPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getWeightedPoolDynamicDataCall {})?;
            // Weights are fixed in the pool's constructor and recorded at creation, so they come
            // from the component rather than from the immutable-data getter.
            let weights =
                bigint_list_attribute(static_attributes, NORMALIZED_WEIGHTS_ATTRIBUTE, pool)?;
            let base = base_state(
                pool,
                pool_type,
                tokens,
                scaling_factors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                aggregate_swap_fee(&config),
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            Ok(PoolState::Weighted(WeightedState::new(base, weights)))
        }
        BalancerPoolType::Stable => {
            let dynamic: StablePoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getStablePoolDynamicDataCall {})?;
            let base = base_state(
                pool,
                pool_type,
                tokens,
                scaling_factors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                aggregate_swap_fee(&config),
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            // The getter reports the amplification already interpolated for the current block, so
            // a pool mid-`AmpUpdate` needs no extra handling here.
            Ok(PoolState::Stable(StableState {
                base,
                mutable: StableMutable { amp: dynamic.amplificationParameter },
            }))
        }
        BalancerPoolType::QuantAmm => {
            let dynamic: QuantAmmPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getQuantAMMWeightedPoolDynamicDataCall {})?;
            let immutable: QuantAmmPoolImmutableData =
                call(engine, pool, IBalancerV3Pool::getQuantAMMWeightedPoolImmutableDataCall {})?;
            let mutable = quantamm_mutable(pool, &dynamic, block_timestamp)?;
            let base = base_state(
                pool,
                pool_type,
                tokens,
                scaling_factors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                config.staticSwapFeePercentage,
                aggregate_swap_fee(&config),
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            Ok(PoolState::QuantAmm(QuantAmmState {
                base,
                mutable,
                immutable: QuantAmmImmutable { max_trade_size_ratio: immutable.maxTradeSizeRatio },
            }))
        }
        BalancerPoolType::Reclamm => {
            let dynamic: ReClammPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getReClammPoolDynamicDataCall {})?;
            let mutable = reclamm_mutable(&dynamic, block_timestamp);
            let base = base_state(
                pool,
                pool_type,
                tokens,
                scaling_factors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                aggregate_swap_fee(&config),
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            Ok(PoolState::ReClammV2(ReClammV2State {
                immutable: ReClammV2Immutable {
                    pool_address: base.pool_address.clone(),
                    tokens: base.tokens.clone(),
                },
                base,
                mutable,
            }))
        }
    }
}

/// Reads the pool's packed config and rejects the states in which it cannot be quoted.
///
/// `Vault.getAggregateSwapFeePercentage` is not implemented on the Vault entrypoint, so the
/// aggregate fee has to come from this config too. An uninitialized or paused pool reverts every
/// swap, and both are recoverable: governance can lift either, and the pause lapses on its own
/// once its window elapses.
fn read_pool_config<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<PoolConfig, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let config: PoolConfig =
        call(engine, &VAULT, IBalancerV3Vault::getPoolConfigCall { pool: *pool })?;
    if !config.isPoolInitialized {
        return Err(SimulationError::RecoverableError(format!(
            "balancer_v3 pool {pool} is not initialized"
        )));
    }
    if is_paused(engine, pool, &config)? {
        return Err(SimulationError::RecoverableError(format!("balancer_v3 pool {pool} is paused")));
    }
    Ok(config)
}

/// The reCLAMM state that moves with time.
///
/// Unlike the other families, the maths recomputes the virtual balances itself from
/// `last_timestamp` to `current_timestamp`, so the quote depends on the block being quoted rather
/// than only on stored state.
fn reclamm_mutable(dynamic: &ReClammPoolDynamicData, block_timestamp: u64) -> ReClammV2Mutable {
    ReClammV2Mutable {
        last_virtual_balances: dynamic.lastVirtualBalances.clone(),
        daily_price_shift_base: dynamic.dailyPriceShiftBase,
        last_timestamp: dynamic.lastTimestamp,
        current_timestamp: U256::from(block_timestamp),
        centeredness_margin: dynamic.centerednessMargin,
        start_fourth_root_price_ratio: dynamic.startFourthRootPriceRatio,
        end_fourth_root_price_ratio: dynamic.endFourthRootPriceRatio,
        price_ratio_update_start_time: U256::from(dynamic.priceRatioUpdateStartTime),
        price_ratio_update_end_time: U256::from(dynamic.priceRatioUpdateEndTime),
    }
}

/// Reads a weighted pool's per-token minimum live balance (`MinTokenBalanceLib`), fixed at
/// registration like the rest of the immutable data [`read_pool_state`] collects.
///
/// The getter was added to the v2 factory generation; earlier weighted pools revert on it, which
/// is read here as "this pool enforces no such minimum" rather than propagated as an error.
pub(super) fn read_weighted_min_token_balances<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Vec<U256>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    call::<D, _, _>(engine, pool, IBalancerV3Pool::getMinTokenBalancesCall {}).unwrap_or_default()
}

/// Re-reads the storage-derived parts of a pool's state, keeping the immutable parts of
/// `previous`.
///
/// Runs on every update, so it skips what registration fixed forever: the hook check, the token
/// list, scaling factors and weights all come from `previous`, which must be a state
/// [`read_pool_state`] produced for the same pool. Only the dynamic data and the pool config —
/// the aggregate fee can move under governance — are read again.
pub(super) fn refresh_pool_state<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    previous: &PoolState,
    block_timestamp: u64,
) -> Result<PoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let config = read_pool_config(engine, pool)?;

    match previous {
        PoolState::Weighted(prev) => {
            let dynamic: WeightedPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getWeightedPoolDynamicDataCall {})?;
            Ok(PoolState::Weighted(WeightedState::new(
                refreshed_base(
                    &prev.base,
                    dynamic.tokenRates,
                    dynamic.balancesLiveScaled18,
                    dynamic.staticSwapFeePercentage,
                    aggregate_swap_fee(&config),
                    dynamic.totalSupply,
                ),
                prev.weights.clone(),
            )))
        }
        PoolState::Stable(prev) => {
            let dynamic: StablePoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getStablePoolDynamicDataCall {})?;
            Ok(PoolState::Stable(StableState {
                base: refreshed_base(
                    &prev.base,
                    dynamic.tokenRates,
                    dynamic.balancesLiveScaled18,
                    dynamic.staticSwapFeePercentage,
                    aggregate_swap_fee(&config),
                    dynamic.totalSupply,
                ),
                mutable: StableMutable { amp: dynamic.amplificationParameter },
            }))
        }
        PoolState::ReClammV2(prev) => {
            let dynamic: ReClammPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getReClammPoolDynamicDataCall {})?;
            let mutable = reclamm_mutable(&dynamic, block_timestamp);
            Ok(PoolState::ReClammV2(ReClammV2State {
                immutable: prev.immutable.clone(),
                base: refreshed_base(
                    &prev.base,
                    dynamic.tokenRates,
                    dynamic.balancesLiveScaled18,
                    dynamic.staticSwapFeePercentage,
                    aggregate_swap_fee(&config),
                    dynamic.totalSupply,
                ),
                mutable,
            }))
        }
        PoolState::QuantAmm(prev) => {
            let dynamic: QuantAmmPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getQuantAMMWeightedPoolDynamicDataCall {})?;
            // The weights and their interpolation window are re-read rather than kept: the pool's
            // rule engine rewrites them whenever it pushes a weight update.
            let mutable = quantamm_mutable(pool, &dynamic, block_timestamp)?;
            Ok(PoolState::QuantAmm(QuantAmmState {
                base: refreshed_base(
                    &prev.base,
                    dynamic.tokenRates,
                    dynamic.balancesLiveScaled18,
                    config.staticSwapFeePercentage,
                    aggregate_swap_fee(&config),
                    dynamic.totalSupply,
                ),
                mutable,
                immutable: prev.immutable.clone(),
            }))
        }
        // `PoolState` covers families this module never builds, so the maths library adding one
        // cannot reach here through a state it produced.
        _ => Err(SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} holds a state of a family this module does not build"
        ))),
    }
}

/// The decimal scaling factors the Vault applies to a pool's raw balances.
///
/// `registerPool` stores `18 - decimals` per token, packed into `tokenDecimalDiffs` at
/// [`DECIMAL_DIFF_BITLENGTH`] bits each, and `PoolConfigLib.getDecimalScalingFactors` expands that
/// back to `10 ** diff`. Both are fixed at registration, so unpacking the config the Vault already
/// reported here saves asking each pool's own immutable-data getter for the same numbers.
fn scaling_factors(config: &PoolConfig, token_count: usize) -> Vec<U256> {
    let packed = config.tokenDecimalDiffs.to::<u64>();
    (0..token_count)
        .map(|index| {
            let diff = (packed >> (index * DECIMAL_DIFF_BITLENGTH)) & DECIMAL_DIFF_MASK;
            U256::from(10u64).pow(U256::from(diff))
        })
        .collect()
}

/// Reads a static attribute holding a JSON list of big-endian integers.
fn bigint_list_attribute(
    static_attributes: &HashMap<String, Bytes>,
    name: &str,
    pool: &AlloyAddress,
) -> Result<Vec<U256>, SimulationError> {
    let raw = static_attributes
        .get(name)
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} carries no `{name}` static attribute; it was indexed with an \
             ethereum-balancer-v3 package that predates it"
        ))
        })?;
    let values = json_deserialize_be_bigint_list(raw).map_err(|e| {
        SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} has a malformed `{name}`: {e}"
        ))
    })?;
    values
        .iter()
        .map(|value| {
            value
                .to_biguint()
                .map(|value| biguint_to_u256(&value))
                .ok_or_else(|| {
                    SimulationError::FatalError(format!(
                        "balancer_v3 pool {pool} reports a negative `{name}` entry: {value}"
                    ))
                })
        })
        .collect()
}

/// The aggregate swap fee the Vault will actually take out of the pool.
///
/// A pool in recovery mode keeps the protocol's share of the swap fee — the Vault skips charging it
/// — so quoting one with its configured percentage would under-report the balance a swap leaves
/// behind. The swapper's `amountOut` is unaffected either way: recovery mode only changes how the
/// fee already charged is split.
fn aggregate_swap_fee(config: &PoolConfig) -> U256 {
    if config.isPoolInRecoveryMode {
        U256::ZERO
    } else {
        config.aggregateSwapFeePercentage
    }
}

/// Whether the Vault would reject a swap on `pool` because it is paused.
///
/// The stored pause bit outlives its effect: `_getPoolPausedState` honours it only while
/// `block.timestamp <= pauseWindowEndTime + _vaultBufferPeriodDuration`, after which a pool left
/// flagged trades again. That buffer is chosen per Vault deployment and no pool getter reports it,
/// so the deadline cannot be applied here and the resolved state is read from the Vault instead —
/// only when the stored bit is set, which for almost every pool it never is.
fn is_paused<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    config: &PoolConfig,
) -> Result<bool, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    if !config.isPoolPaused {
        return Ok(false);
    }
    let state: IBalancerV3Vault::getPoolPausedStateReturn =
        call(engine, &VAULT, IBalancerV3Vault::getPoolPausedStateCall { pool: *pool })?;
    Ok(state._0)
}

/// Assembles the time-dependent half of a QuantAMM pool's state.
///
/// The weights the maths uses are interpolated from `last_update_time` towards `last_interop_time`,
/// so a quote depends on the block being quoted as well as on stored state. The library computes
/// `min(current_timestamp, last_interop_time) - last_update_time` with unchecked arithmetic, so a
/// block that predates the pool's last weight update is rejected here rather than left to
/// underflow inside it.
fn quantamm_mutable(
    pool: &AlloyAddress,
    dynamic: &QuantAmmPoolDynamicData,
    block_timestamp: u64,
) -> Result<QuantAmmMutable, SimulationError> {
    let last_update_time = U256::from(dynamic.lastUpdateTime.to::<u64>());
    let last_interop_time = U256::from(dynamic.lastInteropTime.to::<u64>());
    let current_timestamp = U256::from(block_timestamp);

    let interpolation_time = current_timestamp.min(last_interop_time);
    if interpolation_time < last_update_time {
        return Err(SimulationError::RecoverableError(format!(
            "balancer_v3 QuantAMM pool {pool} was last updated at {last_update_time}, after the \
             {interpolation_time} its weights would be interpolated at"
        )));
    }

    Ok(QuantAmmMutable {
        first_four_weights_and_multipliers: dynamic
            .firstFourWeightsAndMultipliers
            .clone(),
        second_four_weights_and_multipliers: dynamic
            .secondFourWeightsAndMultipliers
            .clone(),
        last_update_time,
        last_interop_time,
        current_timestamp,
    })
}

/// Overwrites the storage-derived fields of `previous` with freshly read values.
///
/// `supports_unbalanced_liquidity` stays: liquidity management is fixed at registration.
fn refreshed_base(
    previous: &BasePoolState,
    token_rates: Vec<U256>,
    balances_live_scaled_18: Vec<U256>,
    swap_fee: U256,
    aggregate_swap_fee: U256,
    total_supply: U256,
) -> BasePoolState {
    BasePoolState {
        token_rates,
        balances_live_scaled_18,
        swap_fee,
        aggregate_swap_fee,
        total_supply,
        ..previous.clone()
    }
}

/// Assembles the fields every pool family shares.
#[allow(clippy::too_many_arguments)]
fn base_state(
    pool: &AlloyAddress,
    pool_type: BalancerPoolType,
    tokens: Vec<String>,
    scaling_factors: Vec<U256>,
    token_rates: Vec<U256>,
    balances_live_scaled_18: Vec<U256>,
    swap_fee: U256,
    aggregate_swap_fee: U256,
    total_supply: U256,
    liquidity_management: &LiquidityManagement,
) -> BasePoolState {
    BasePoolState {
        pool_address: format!("{pool:?}"),
        pool_type: pool_type.maths_marker().to_string(),
        tokens,
        scaling_factors,
        token_rates,
        balances_live_scaled_18,
        swap_fee,
        aggregate_swap_fee,
        total_supply,
        supports_unbalanced_liquidity: !liquidity_management.disableUnbalancedLiquidity,
        hook_type: None,
    }
}

fn params(to: AlloyAddress, data: Vec<u8>) -> SimulationParameters {
    SimulationParameters {
        caller: AlloyAddress::ZERO,
        to,
        data,
        value: U256::ZERO,
        overrides: None,
        gas_limit: None,
        transient_storage: None,
        block_overrides: None,
        gas_price: None,
    }
}

fn call<D, C, R>(
    engine: &SimulationEngine<D>,
    to: &AlloyAddress,
    sol_call: C,
) -> Result<R, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    C: SolCall<Return = R>,
{
    let res = engine
        .simulate(&params(*to, sol_call.abi_encode()))
        .map_err(|e| {
            SimulationError::RecoverableError(format!("balancer_v3 getter call failed: {e}"))
        })?;
    C::abi_decode_returns(res.result.as_ref())
        .map_err(|e| SimulationError::FatalError(format!("balancer_v3 getter decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attributes(pool_type: &str) -> HashMap<String, Bytes> {
        HashMap::from([(
            POOL_TYPE_ATTRIBUTE.to_string(),
            Bytes::from(pool_type.as_bytes().to_vec()),
        )])
    }

    /// A pool config carrying only the fields [`aggregate_swap_fee`] reads.
    fn config_with(aggregate_fee: u64, in_recovery_mode: bool) -> PoolConfig {
        PoolConfig {
            liquidityManagement: LiquidityManagement {
                disableUnbalancedLiquidity: false,
                enableAddLiquidityCustom: false,
                enableRemoveLiquidityCustom: false,
                enableDonation: false,
            },
            staticSwapFeePercentage: U256::ZERO,
            aggregateSwapFeePercentage: U256::from(aggregate_fee),
            aggregateYieldFeePercentage: U256::ZERO,
            tokenDecimalDiffs: Default::default(),
            pauseWindowEndTime: 0,
            isPoolRegistered: true,
            isPoolInitialized: true,
            isPoolPaused: false,
            isPoolInRecoveryMode: in_recovery_mode,
        }
    }

    /// `registerPool` packs `18 - decimals` per token, least significant field first. Reproduces
    /// what the Vault would report for a USDC/WETH/WBTC pool registered in that order.
    #[test]
    fn scaling_factors_unpack_the_registered_token_decimals() {
        let mut config = config_with(0, false);
        let diffs = [12u64, 0, 10]; // 6, 18 and 8 decimals
        let packed = diffs
            .iter()
            .enumerate()
            .fold(0u64, |packed, (index, diff)| {
                packed | (diff << (index * DECIMAL_DIFF_BITLENGTH))
            });
        config.tokenDecimalDiffs = packed
            .try_into()
            .expect("three diffs fit a uint40");

        assert_eq!(
            scaling_factors(&config, 3),
            vec![U256::from(1_000_000_000_000u64), U256::from(1u64), U256::from(10_000_000_000u64)]
        );
        // A pool with fewer tokens must not read its neighbours' fields.
        assert_eq!(scaling_factors(&config, 1), vec![U256::from(1_000_000_000_000u64)]);
    }

    /// Recovery mode leaves the protocol's share of the swap fee in the pool, so quoting one must
    /// not deduct it — the Vault does not charge it there.
    #[test]
    fn recovery_mode_waives_the_aggregate_swap_fee() {
        let charged = config_with(250_000_000_000_000_000, false);
        assert_eq!(aggregate_swap_fee(&charged), U256::from(250_000_000_000_000_000u64));

        let waived = config_with(250_000_000_000_000_000, true);
        assert_eq!(aggregate_swap_fee(&waived), U256::ZERO);
    }

    #[test]
    fn maps_factory_markers_to_pool_types() {
        assert_eq!(BalancerPoolType::Weighted.factory_marker(), "WeightedPoolFactory");
        assert_eq!(BalancerPoolType::Stable.factory_marker(), "StablePoolFactory");
        assert_eq!(BalancerPoolType::Reclamm.factory_marker(), "ReClammPoolFactory");
        assert_eq!(BalancerPoolType::QuantAmm.factory_marker(), "QuantAMMWeightedPoolFactory");
        assert_eq!(BalancerPoolType::Weighted.maths_marker(), "WEIGHTED");
        assert_eq!(BalancerPoolType::Stable.maths_marker(), "STABLE");
        assert_eq!(BalancerPoolType::Reclamm.maths_marker(), "RECLAMM_V2");
        assert_eq!(BalancerPoolType::QuantAmm.maths_marker(), "QUANT_AMM_WEIGHTED");
    }

    #[test]
    fn resolves_every_supported_factory_marker() {
        for expected in BalancerPoolType::ALL {
            let attrs = attributes(expected.factory_marker());
            let raw = attrs
                .get(POOL_TYPE_ATTRIBUTE)
                .expect("attribute present");
            let marker = String::from_utf8(raw.to_vec()).expect("utf-8");
            assert_eq!(
                BalancerPoolType::from_factory_marker(&marker),
                Some(expected),
                "`{marker}` must resolve back to the family that emits it"
            );
        }
    }

    #[test]
    fn rejects_families_the_maths_cannot_price() {
        for marker in ["GyroECLPPoolFactory", "LBPoolFactory"] {
            assert_eq!(BalancerPoolType::from_factory_marker(marker), None);
        }
    }
}
