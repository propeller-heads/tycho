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
    primitives::{Address as AlloyAddress, U256},
    sol_types::SolCall,
};
use balancer_maths_rust::{
    common::types::{BasePoolState, PoolState},
    pools::{
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
    struct WeightedPoolImmutableData {
        address[] tokens;
        uint256[] decimalScalingFactors;
        uint256[] normalizedWeights;
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
    struct StablePoolImmutableData {
        address[] tokens;
        uint256[] decimalScalingFactors;
        uint256 amplificationParameterPrecision;
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
    struct ReClammPoolImmutableData {
        address[] tokens;
        uint256[] decimalScalingFactors;
        bool tokenAPriceIncludesRate;
        bool tokenBPriceIncludesRate;
        uint256 minSwapFeePercentage;
        uint256 maxSwapFeePercentage;
        uint256 initialMinPrice;
        uint256 initialMaxPrice;
        uint256 initialTargetPrice;
        uint256 initialDailyPriceShiftExponent;
        uint256 initialCenterednessMargin;
        uint256 minPriceRatio;
        uint256 maxPriceRatio;
        uint256 maxCenterednessMargin;
        uint256 maxDailyPriceShiftExponent;
        uint256 maxDailyPriceRatioUpdateRate;
        uint256 minPriceRatioUpdateDuration;
        uint256 minPriceRatioDelta;
        uint256 balanceRatioAndPriceTolerance;
    }

    #[allow(missing_docs)]
    interface IBalancerV3Pool {
        function getVault() external view returns (address);
        function getWeightedPoolDynamicData() external view returns (WeightedPoolDynamicData memory);
        function getWeightedPoolImmutableData() external view returns (WeightedPoolImmutableData memory);
        function getStablePoolDynamicData() external view returns (StablePoolDynamicData memory);
        function getStablePoolImmutableData() external view returns (StablePoolImmutableData memory);
        function getReClammPoolDynamicData() external view returns (ReClammPoolDynamicData memory);
        function getReClammPoolImmutableData() external view returns (ReClammPoolImmutableData memory);
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

/// `pool_type` static attribute emitted by the `ethereum-balancer-v3` Substreams package.
const POOL_TYPE_ATTRIBUTE: &str = "pool_type";

/// The pool families this module can quote natively.
///
/// Balancer V3 exposes far more (Gyro, QuantAMM, LBP, and anything built on the hook system);
/// those are deliberately rejected at decode time rather than approximated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BalancerPoolType {
    /// Constant weighted product pools.
    Weighted,
    /// StableMath pools, including pools whose amplification is mid-update.
    Stable,
    /// AutoRange pools, whose price range shifts with time.
    Reclamm,
}

impl BalancerPoolType {
    /// The `pool_type` attribute value the Substreams package writes for this family.
    fn factory_marker(self) -> &'static str {
        match self {
            Self::Weighted => "WeightedPoolFactory",
            Self::Stable => "StablePoolFactory",
            Self::Reclamm => "ReClammPoolFactory",
        }
    }

    /// The `pool_type` string `balancer_maths_rust` keys its pool implementations on.
    ///
    /// The V3-generation pools we index share their swap maths with the library's V2
    /// implementation, which Balancer confirmed, so both map onto `RECLAMM_V2`.
    fn maths_marker(self) -> &'static str {
        match self {
            Self::Weighted => "WEIGHTED",
            Self::Stable => "STABLE",
            Self::Reclamm => "RECLAMM_V2",
        }
    }
}

/// Determines which pool family `pool` belongs to.
///
/// Prefers the `pool_type` static attribute and falls back to probing the type-specific getter,
/// so pools indexed before the attribute existed still resolve. Returns an error for any family
/// this module cannot quote, which keeps such pools out of the native decoder instead of letting
/// them be priced by the wrong maths.
pub(super) fn resolve_pool_type<D: EngineDatabaseInterface + Clone + Debug>(
    static_attributes: &HashMap<String, Bytes>,
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<BalancerPoolType, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    if let Some(raw) = static_attributes.get(POOL_TYPE_ATTRIBUTE) {
        let marker = String::from_utf8(raw.to_vec()).map_err(|e| {
            SimulationError::FatalError(format!("balancer_v3 pool_type is not UTF-8: {e}"))
        })?;
        for candidate in [BalancerPoolType::Weighted, BalancerPoolType::Stable] {
            if candidate.factory_marker() == marker {
                return Ok(candidate);
            }
        }
        return Err(SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} has unsupported pool_type `{marker}`"
        )));
    }

    if call::<D, _, _>(engine, pool, IBalancerV3Pool::getWeightedPoolImmutableDataCall {}).is_ok() {
        return Ok(BalancerPoolType::Weighted);
    }
    if call::<D, _, _>(engine, pool, IBalancerV3Pool::getStablePoolImmutableDataCall {}).is_ok() {
        return Ok(BalancerPoolType::Stable);
    }
    if call::<D, _, _>(engine, pool, IBalancerV3Pool::getReClammPoolImmutableDataCall {}).is_ok() {
        return Ok(BalancerPoolType::Reclamm);
    }
    Err(SimulationError::FatalError(format!(
        "balancer_v3 pool {pool} exposes none of the weighted, stable or reCLAMM getters"
    )))
}

/// Reads the Vault this pool is registered with.
pub(super) fn read_vault<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<AlloyAddress, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    call(engine, pool, IBalancerV3Pool::getVaultCall {})
}

/// Builds the maths library's pool state for `pool` from the indexed storage.
///
/// The returned state is a snapshot for the block the engine's database is at; callers rebuild it
/// on every update rather than applying deltas to it. `block_timestamp` is the timestamp quotes
/// should be evaluated at — reCLAMM shifts its price range with time, so it needs the current
/// block's timestamp rather than the pool's last-updated one.
pub(super) fn read_pool_state<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    vault: &AlloyAddress,
    pool_type: BalancerPoolType,
    block_timestamp: u64,
) -> Result<PoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    // `Vault.getAggregateSwapFeePercentage` is not implemented on the Vault entrypoint, so the
    // aggregate fee has to come from the packed pool config.
    let config: PoolConfig =
        call(engine, vault, IBalancerV3Vault::getPoolConfigCall { pool: *pool })?;
    if !config.isPoolInitialized {
        return Err(SimulationError::RecoverableError(format!(
            "balancer_v3 pool {pool} is not initialized"
        )));
    }

    // The factory a pool came from says nothing about its hooks: a StablePoolFactory pool can carry
    // a dynamic-fee hook, and quoting it as hookless yields amounts the Vault rejects.
    let hooks: HooksConfig =
        call(engine, vault, IBalancerV3Vault::getHooksConfigCall { pool: *pool })?;
    if hooks.affects_swaps() {
        return Err(SimulationError::FatalError(format!(
            "balancer_v3 pool {pool} uses swap hook {:?}, which the native maths does not model",
            hooks.hooksContract
        )));
    }

    match pool_type {
        BalancerPoolType::Weighted => {
            let dynamic: WeightedPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getWeightedPoolDynamicDataCall {})?;
            let immutable: WeightedPoolImmutableData =
                call(engine, pool, IBalancerV3Pool::getWeightedPoolImmutableDataCall {})?;
            let base = base_state(
                pool,
                pool_type,
                &immutable.tokens,
                immutable.decimalScalingFactors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                config.aggregateSwapFeePercentage,
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            Ok(PoolState::Weighted(WeightedState::new(base, immutable.normalizedWeights)))
        }
        BalancerPoolType::Stable => {
            let dynamic: StablePoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getStablePoolDynamicDataCall {})?;
            let immutable: StablePoolImmutableData =
                call(engine, pool, IBalancerV3Pool::getStablePoolImmutableDataCall {})?;
            let base = base_state(
                pool,
                pool_type,
                &immutable.tokens,
                immutable.decimalScalingFactors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                config.aggregateSwapFeePercentage,
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
        BalancerPoolType::Reclamm => {
            let dynamic: ReClammPoolDynamicData =
                call(engine, pool, IBalancerV3Pool::getReClammPoolDynamicDataCall {})?;
            let immutable: ReClammPoolImmutableData =
                call(engine, pool, IBalancerV3Pool::getReClammPoolImmutableDataCall {})?;
            let base = base_state(
                pool,
                pool_type,
                &immutable.tokens,
                immutable.decimalScalingFactors,
                dynamic.tokenRates,
                dynamic.balancesLiveScaled18,
                dynamic.staticSwapFeePercentage,
                config.aggregateSwapFeePercentage,
                dynamic.totalSupply,
                &config.liquidityManagement,
            );
            // Unlike the other families, the maths recomputes the virtual balances itself from
            // `last_timestamp` to `current_timestamp`, so the quote depends on the block being
            // quoted rather than only on stored state.
            Ok(PoolState::ReClammV2(ReClammV2State {
                immutable: ReClammV2Immutable {
                    pool_address: base.pool_address.clone(),
                    tokens: base.tokens.clone(),
                },
                base,
                mutable: ReClammV2Mutable {
                    last_virtual_balances: dynamic.lastVirtualBalances,
                    daily_price_shift_base: dynamic.dailyPriceShiftBase,
                    last_timestamp: dynamic.lastTimestamp,
                    current_timestamp: U256::from(block_timestamp),
                    centeredness_margin: dynamic.centerednessMargin,
                    start_fourth_root_price_ratio: dynamic.startFourthRootPriceRatio,
                    end_fourth_root_price_ratio: dynamic.endFourthRootPriceRatio,
                    price_ratio_update_start_time: U256::from(dynamic.priceRatioUpdateStartTime),
                    price_ratio_update_end_time: U256::from(dynamic.priceRatioUpdateEndTime),
                },
            }))
        }
    }
}

/// Assembles the fields every pool family shares.
#[allow(clippy::too_many_arguments)]
fn base_state(
    pool: &AlloyAddress,
    pool_type: BalancerPoolType,
    tokens: &[AlloyAddress],
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
        tokens: tokens
            .iter()
            .map(|token| format!("{token:?}"))
            .collect(),
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

    #[test]
    fn maps_factory_markers_to_pool_types() {
        assert_eq!(BalancerPoolType::Weighted.factory_marker(), "WeightedPoolFactory");
        assert_eq!(BalancerPoolType::Stable.factory_marker(), "StablePoolFactory");
        assert_eq!(BalancerPoolType::Reclamm.factory_marker(), "ReClammPoolFactory");
        assert_eq!(BalancerPoolType::Weighted.maths_marker(), "WEIGHTED");
        assert_eq!(BalancerPoolType::Stable.maths_marker(), "STABLE");
        assert_eq!(BalancerPoolType::Reclamm.maths_marker(), "RECLAMM_V2");
    }

    #[test]
    fn attributes_carry_enough_to_resolve_supported_types() {
        for (marker, expected) in [
            ("WeightedPoolFactory", BalancerPoolType::Weighted),
            ("StablePoolFactory", BalancerPoolType::Stable),
            ("ReClammPoolFactory", BalancerPoolType::Reclamm),
        ] {
            let attrs = attributes(marker);
            let raw = attrs
                .get(POOL_TYPE_ATTRIBUTE)
                .expect("attribute present");
            let decoded = String::from_utf8(raw.to_vec()).expect("utf-8");
            assert_eq!(decoded, expected.factory_marker());
        }
    }
}
