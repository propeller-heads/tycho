//! [`BalancerV3State`] — a hybrid Balancer V3 pool: pure-Rust quote maths
//! (`balancer_maths_rust`) over state read from the locally indexed VM storage.
use std::any::Any;

use alloy::primitives::{Address as AlloyAddress, U256};
use balancer_maths_rust::{
    common::{
        maths::{div_up_fixed, mul_down_fixed, mul_up_fixed, pow_up_fixed},
        pool_base::PoolBase,
        types::{PoolState, SwapInput, SwapKind, SwapParams},
        utils::{
            compute_and_charge_aggregate_swap_fees, to_raw_undo_rate_round_down,
            to_scaled_18_apply_rate_round_down,
        },
        WAD as ONE_WAD_SCALED_18,
    },
    pools::{
        quantamm::QuantAmmPool,
        reclammv2::{compute_current_virtual_balances, compute_in_given_out, ReClammV2Pool},
        stable::{self, StablePool},
        weighted::{WeightedPool, MAX_IN_RATIO},
    },
    vault::swap::{swap as vault_swap, MINIMUM_TRADE_AMOUNT},
    DefaultHook, PoolError,
};
use num_bigint::{BigUint, ToBigUint};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        balancer_v3::vm::{self, BalancerPoolType, PoolTypeAttribute},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::add_fee_markup,
    },
};

/// Fee and rate denominator used throughout Balancer V3 (`1e18`).
const WAD: f64 = 1e18;
/// Representative gas for a single-hop Balancer V3 swap. Executions observed in the
/// `vm:balancer_v3` integration test spent 206k–242k gas including the router and executor.
const SWAP_GAS: u64 = 210_000;
/// Fraction of the input balance probed to approximate the marginal price (`1e-6`).
const SPOT_PRICE_PROBE_DIVISOR: u64 = 1_000_000;
/// Attribute the stream decoder attaches to every delta, carrying the block's timestamp.
const BLOCK_TIMESTAMP_ATTRIBUTE: &str = "block_timestamp";
/// Hard cap the Vault stores any balance under (`2^128 - 1`), which is what bounds a stable
/// pool's input: `StableMath` itself has no input limit.
const MAX_VAULT_BALANCE: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
/// Largest share of the output reserve a reCLAMM swap may buy (`0.99e18`), matching
/// `_MAX_TOKEN_OUT_RATIO` in the reference implementation.
const MAX_TOKEN_OUT_RATIO: U256 = U256::from_limbs([990_000_000_000_000_000, 0, 0, 0]);
/// Largest ratio between any two live balances a stable-pool swap may leave behind, matching
/// `StableMath.MAX_IMBALANCE_RATIO` (`10_000`). Added to the v3 factory generation's
/// `StablePool.onSwap`; `balancer_maths_rust` still only models the December 2024 genesis
/// contracts and has no such check.
const STABLE_MAX_IMBALANCE_RATIO: U256 = U256::from_limbs([10_000, 0, 0, 0]);

/// A single Balancer V3 pool quoted through `balancer_maths_rust`.
///
/// `tokens` follows the pool's own registration order, which is what the maths library indexes
/// balances, rates and weights by. The storage-derived parts of the state are re-read from the VM
/// on every [`ProtocolSim::delta_transition`] rather than patched from the delta, because the
/// values the maths needs (live balances, token rates, amplification) are derived from storage
/// that other contracts — rate providers above all — own. What registration fixed forever
/// (tokens, scaling factors, weights, the hook check) is kept from decode time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BalancerV3State {
    /// Pool contract address (the Tycho component id).
    pool_address: Bytes,
    /// Vault the pool is registered with, kept so updates need no extra lookup.
    vault: Bytes,
    /// Resolved pool family.
    pool_type: BalancerPoolType,
    /// Factory generation this pool was created by, as labelled in the Substreams deployment
    /// params. `None` for pools indexed before generations were labelled. Every generation of a
    /// family shares the same maths, so this never affects quoting.
    factory_version: Option<String>,
    /// Token addresses in pool registration order.
    tokens: Vec<Bytes>,
    /// Per-token minimum live balance (scaled 18, pool registration order) a weighted pool's own
    /// `MinTokenBalanceLib` check enforces. Empty for non-weighted pools and for weighted pools
    /// from factory generations that predate the check (`getMinTokenBalances` reverts on them),
    /// in which case no such floor applies. Fixed at registration, so it is read once at decode
    /// time like the other immutable fields.
    min_token_balances: Vec<U256>,
    /// Timestamp of the block this state was read at. reCLAMM quotes depend on it, so it is
    /// refreshed on every update; the other families ignore it.
    block_timestamp: u64,
    /// Pool state in the form the maths library consumes.
    state: PoolState,
}

impl BalancerV3State {
    /// Constructs a state from a resolved `pool_type` attribute and a state read out of the VM.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        pool_address: Bytes,
        vault: Bytes,
        factory: PoolTypeAttribute,
        tokens: Vec<Bytes>,
        min_token_balances: Vec<U256>,
        block_timestamp: u64,
        state: PoolState,
    ) -> Self {
        let PoolTypeAttribute { pool_type, version } = factory;
        Self {
            pool_address,
            vault,
            pool_type,
            factory_version: version,
            tokens,
            min_token_balances,
            block_timestamp,
            state,
        }
    }

    /// The factory generation this pool was created by, when the indexer labelled one.
    pub fn factory_version(&self) -> Option<&str> {
        self.factory_version.as_deref()
    }

    /// Token addresses in pool registration order.
    #[cfg(test)]
    pub(super) fn token_addresses(&self) -> &[Bytes] {
        &self.tokens
    }

    /// Reserves in each token's own units, in pool registration order.
    #[cfg(test)]
    pub(super) fn raw_balances(&self) -> Vec<U256> {
        let base = self.state.base();
        (0..base.balances_live_scaled_18.len())
            .map(|index| raw_balance(base, index).expect("a live balance must rescale to raw"))
            .collect()
    }

    /// Live scaled-18 balances in pool registration order.
    #[cfg(test)]
    pub(super) fn state_balances(&self) -> &[U256] {
        &self
            .state
            .base()
            .balances_live_scaled_18
    }

    pub(super) fn token_index(&self, token: &Bytes) -> Result<usize, SimulationError> {
        self.tokens
            .iter()
            .position(|candidate| candidate == token)
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!(
                        "token {token} is not registered in balancer_v3 pool {}",
                        self.pool_address
                    ),
                    None,
                )
            })
    }

    /// Renders an address the way the maths library expects it (lowercase, `0x`-prefixed).
    fn token_key(token: &Bytes) -> String {
        format!("0x{}", hex::encode(token))
    }

    /// Builds the pool implementation the maths dispatches a swap to.
    ///
    /// This mirrors what `Vault::swap` does internally, and is built here so that swapping can go
    /// through [`vault::swap::swap`] against a borrowed state. [`Self::spot_price`] needs it for a
    /// second reason: only `on_swap` reports an amount before the swap fee is taken.
    fn pool_impl(&self) -> Result<Box<dyn PoolBase>, PoolError> {
        match &self.state {
            PoolState::Weighted(state) => Ok(Box::new(WeightedPool::from(state.clone()))),
            PoolState::Stable(state) => Ok(Box::new(StablePool::new(state.mutable.clone()))),
            PoolState::ReClammV2(state) => Ok(Box::new(ReClammV2Pool::new(state.clone()))),
            // Unlike the other families, building this resolves the pool's time-interpolated
            // weights, which fails if the packed weight arrays are shorter than the token list.
            PoolState::QuantAmm(state) => {
                QuantAmmPool::new(state.clone()).map(|pool| Box::new(pool) as Box<dyn PoolBase>)
            }
            other => Err(PoolError::UnsupportedPoolType(other.pool_type().to_string())),
        }
    }

    /// Swaps `amount_in` of `token_in` for `token_out`, returning the Vault's own [`PoolError`] on
    /// failure so callers that care about a specific failure mode (see
    /// [`ProtocolSim::get_limits`]) do not have to parse it back out of a formatted message.
    fn vault_swap_exact_in(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<U256, PoolError> {
        let input = SwapInput {
            amount_raw: amount_in,
            swap_kind: SwapKind::GivenIn,
            token_in: Self::token_key(token_in),
            token_out: Self::token_key(token_out),
        };
        // `Vault::swap` would take the state by `Box<PoolState>`, forcing a full clone of it on
        // every call — which the limit searches make hundreds of. Its body only builds the pool
        // implementation and the hook before delegating here, so both are supplied directly and
        // the state is borrowed. Pools carrying a swap hook never reach this far: the decoder
        // rejects them, leaving the maths library's own no-op hook as the faithful choice.
        vault_swap(&input, &self.state, self.pool_impl()?.as_ref(), &DefaultHook::new(), None)
    }

    /// Swaps `amount_in` of `token_in` for `token_out`, returning the raw output amount.
    fn swap_exact_in(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<U256, SimulationError> {
        self.vault_swap_exact_in(amount_in, token_in, token_out)
            .map_err(|e| {
                SimulationError::RecoverableError(format!(
                    "balancer_v3 swap failed for pool {}: {e:?}",
                    self.pool_address
                ))
            })
    }

    /// Largest input the Vault accepts for a swap from `index_in` to `index_out`, in the input
    /// token's raw units.
    ///
    /// Mirrors the reference implementation's `getMaxSwapAmount` for exact-in swaps. Weighted
    /// pools cap the input at [`MAX_IN_RATIO`] of the input reserve (`WeightedMath`, enforced on
    /// every generation) and, for pools that register one, a per-token minimum balance (see
    /// [`Self::weighted_max_swap_amount_in`]). Stable pools are capped by
    /// [`Self::stable_max_swap_amount_in`]. reCLAMM pools are bounded by the output side: the
    /// limit is the input that buys [`MAX_TOKEN_OUT_RATIO`] of the output reserve at the current
    /// virtual balances.
    fn max_swap_amount_in(
        &self,
        index_in: usize,
        index_out: usize,
    ) -> Result<U256, SimulationError> {
        let base = self.state.base();
        let balances = &base.balances_live_scaled_18;
        let maths_error = |e: PoolError| {
            SimulationError::FatalError(format!(
                "balancer_v3 swap limit failed for pool {}: {e:?}",
                self.pool_address
            ))
        };

        let max_in_scaled_18 = match &self.state {
            PoolState::Weighted(state) => {
                self.weighted_max_swap_amount_in(index_in, index_out, state.weights())?
            }
            PoolState::Stable(_) => self.stable_max_swap_amount_in(index_in, index_out)?,
            // QuantAMM is bounded in raw units rather than scaled-18 ones, so it returns straight
            // away instead of falling through to the conversion below.
            PoolState::QuantAmm(state) => {
                return self.quantamm_max_swap_amount_in(
                    index_in,
                    index_out,
                    &state.immutable.max_trade_size_ratio,
                )
            }
            PoolState::ReClammV2(state) => {
                let max_out_scaled_18 = mul_down_fixed(&MAX_TOKEN_OUT_RATIO, &balances[index_out])
                    .map_err(maths_error)?;
                let mutable = &state.mutable;
                let (virtual_balance_a, virtual_balance_b, _) = compute_current_virtual_balances(
                    &mutable.current_timestamp,
                    balances,
                    &mutable.last_virtual_balances[0],
                    &mutable.last_virtual_balances[1],
                    &mutable.daily_price_shift_base,
                    &mutable.last_timestamp,
                    &mutable.centeredness_margin,
                    &mutable.start_fourth_root_price_ratio,
                    &mutable.end_fourth_root_price_ratio,
                    &mutable.price_ratio_update_start_time,
                    &mutable.price_ratio_update_end_time,
                );
                compute_in_given_out(
                    balances,
                    &virtual_balance_a,
                    &virtual_balance_b,
                    index_in,
                    index_out,
                    &max_out_scaled_18,
                )
                .map_err(|e| {
                    SimulationError::FatalError(format!(
                        "balancer_v3 swap limit failed for pool {}: {e}",
                        self.pool_address
                    ))
                })?
            }
            other => {
                return Err(SimulationError::FatalError(format!(
                    "balancer_v3 pool {} holds unsupported state `{}`",
                    self.pool_address,
                    other.pool_type()
                )))
            }
        };

        to_raw_undo_rate_round_down(
            &max_in_scaled_18,
            &base.scaling_factors[index_in],
            &base.token_rates[index_in],
        )
        .map_err(maths_error)
    }

    /// Largest raw input a QuantAMM pool accepts, found by binary search over the Vault path.
    ///
    /// `onSwap` applies `maxTradeSizeRatio` to *both* sides of the trade, so the input reserve's
    /// share of it is only an upper bound — on a pool whose weights are far apart, a permitted
    /// input can still buy more of the output reserve than the same ratio allows. Inverting the
    /// output bound in closed form would mean recomputing the pool's time-interpolated weights
    /// here, duplicating logic that lives in `balancer_maths_rust` and would silently drift from
    /// the quotes it produces; probing the real swap instead keeps the limit consistent with
    /// [`Self::get_amount_out`] by construction. The predicate is monotonic in the input, so the
    /// search converges on the largest amount the Vault accepts, and reports zero when it accepts
    /// none.
    fn quantamm_max_swap_amount_in(
        &self,
        index_in: usize,
        index_out: usize,
        max_trade_size_ratio: &U256,
    ) -> Result<U256, SimulationError> {
        let base = self.state.base();
        let maths_error = |e: PoolError| {
            SimulationError::FatalError(format!(
                "balancer_v3 swap limit failed for pool {}: {e:?}",
                self.pool_address
            ))
        };

        let input_cap_scaled_18 =
            mul_down_fixed(&base.balances_live_scaled_18[index_in], max_trade_size_ratio)
                .map_err(maths_error)?;
        let mut high = to_raw_undo_rate_round_down(
            &input_cap_scaled_18,
            &base.scaling_factors[index_in],
            &base.token_rates[index_in],
        )
        .map_err(maths_error)?;

        let (token_in, token_out) = (&self.tokens[index_in], &self.tokens[index_out]);
        let accepted = |amount: &U256| {
            self.vault_swap_exact_in(*amount, token_in, token_out)
                .is_ok()
        };
        if accepted(&high) {
            return Ok(high);
        }

        let mut low = U256::ZERO;
        while high - low > U256::from(1) {
            let mid = low + ((high - low) >> 1);
            if accepted(&mid) {
                low = mid;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }

    /// Caps a weighted-pool exact-in swap in scaled-18 terms, the way the Vault would reject it
    /// on-chain.
    ///
    /// Every generation enforces [`MAX_IN_RATIO`] inside `WeightedMath.computeOutGivenExactIn`.
    /// Pools registering a per-token minimum balance (`MinTokenBalanceLib`, added to the v2
    /// factory generation and not modelled by `balancer_maths_rust`) additionally require that
    /// neither token's balance fall below its own minimum after the swap; that bound is inverted
    /// through [`weighted_in_given_exact_out_unguarded`]. Both caps apply, so the tighter one
    /// wins.
    fn weighted_max_swap_amount_in(
        &self,
        index_in: usize,
        index_out: usize,
        weights: &[U256],
    ) -> Result<U256, SimulationError> {
        let base = self.state.base();
        let balances = &base.balances_live_scaled_18;
        let maths_error = |e: PoolError| {
            SimulationError::FatalError(format!(
                "balancer_v3 swap limit failed for pool {}: {e:?}",
                self.pool_address
            ))
        };

        let ratio_cap = mul_down_fixed(&balances[index_in], &MAX_IN_RATIO).map_err(maths_error)?;
        let (Some(&min_in), Some(&min_out)) =
            (self.min_token_balances.get(index_in), self.min_token_balances.get(index_out))
        else {
            // No factory-registered minimum for this pool: nothing beyond MAX_IN_RATIO applies.
            return Ok(ratio_cap);
        };

        // Mirrors `onSwap`'s check on the input side: it reads the current balance (offset by the
        // Vault's rounding buffer of 1), not a post-swap one, so no amount can make this pass.
        if balances[index_in] + U256::from(1) < min_in {
            return Ok(U256::ZERO);
        }
        // A zero minimum registers no real floor for this token: skip the inversion below rather
        // than feed it a target of the full balance, which is a singular point on the curve
        // (`weighted_in_given_exact_out_unguarded` divides by `balance_out - target_out`).
        if min_out.is_zero() {
            return Ok(ratio_cap);
        }
        let Some(target_out) = balances[index_out].checked_sub(min_out) else {
            return Ok(U256::ZERO);
        };
        if target_out.is_zero() {
            return Ok(U256::ZERO);
        }

        let min_balance_cap = weighted_in_given_exact_out_unguarded(
            &balances[index_in],
            &weights[index_in],
            &balances[index_out],
            &weights[index_out],
            &target_out,
        )
        .map_err(maths_error)?;
        Ok(ratio_cap.min(min_balance_cap))
    }

    /// Largest exact-in input a stable-pool swap can take without the live balances drifting past
    /// [`StableMath.ensureBalancesWithinMaxImbalanceRange`][`STABLE_MAX_IMBALANCE_RATIO`], found by
    /// binary search since that check has no closed-form inverse over the stable invariant.
    /// [`Self::stable_swap_keeps_balance_valid`] is monotonic in the input amount, so the search
    /// converges to within a wei.
    pub(super) fn stable_max_swap_amount_in(
        &self,
        index_in: usize,
        index_out: usize,
    ) -> Result<U256, SimulationError> {
        let balances = &self
            .state
            .base()
            .balances_live_scaled_18;
        let mut low = U256::ZERO;
        let mut high = MAX_VAULT_BALANCE.saturating_sub(balances[index_in]);
        if self.stable_swap_keeps_balance_valid(index_in, index_out, &high)? {
            return Ok(high);
        }
        while high - low > U256::from(1) {
            let mid = low + ((high - low) >> 1);
            if self.stable_swap_keeps_balance_valid(index_in, index_out, &mid)? {
                low = mid;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }

    /// Whether an exact-in swap of `amount_in_scaled_18` (pre-fee, matching the Vault's
    /// `amountGivenScaled18` before the swap-fee deduction) leaves every live balance inside the
    /// pool's maximum imbalance ratio. Mirrors the v3-generation `StablePool.onSwap`, which
    /// `balancer_maths_rust` — modelling only the December 2024 genesis contracts — does not
    /// check.
    pub(super) fn stable_swap_keeps_balance_valid(
        &self,
        index_in: usize,
        index_out: usize,
        amount_in_scaled_18: &U256,
    ) -> Result<bool, SimulationError> {
        let base = self.state.base();
        let PoolState::Stable(state) = &self.state else {
            return Err(SimulationError::FatalError(format!(
                "balancer_v3 pool {} is not a stable pool",
                self.pool_address
            )));
        };
        let balances = &base.balances_live_scaled_18;
        let maths_error = |e: PoolError| {
            SimulationError::FatalError(format!(
                "balancer_v3 stable limit probe failed for pool {}: {e:?}",
                self.pool_address
            ))
        };

        // `stable_math::compute_invariant` divides by each balance directly (not through a
        // checked helper), so a zero balance on a token this swap does not even touch — possible
        // in a pool with more than two tokens, since `get_limits` only screens `index_in` and
        // `index_out` — would panic rather than error. Such a pool cannot be swapped through at
        // all until re-seeded.
        if balances.iter().any(U256::is_zero) {
            return Ok(false);
        }

        let fee_scaled = mul_up_fixed(amount_in_scaled_18, &base.swap_fee).map_err(maths_error)?;
        let Some(amount_in_after_fee) = amount_in_scaled_18.checked_sub(fee_scaled) else {
            return Ok(false);
        };
        if amount_in_after_fee < MINIMUM_TRADE_AMOUNT {
            return Ok(false);
        }

        let amp = &state.mutable.amp;
        let invariant = stable::compute_invariant(amp, balances).map_err(maths_error)?;
        let Ok(amount_out_scaled) = stable::compute_out_given_exact_in(
            amp,
            balances,
            index_in,
            index_out,
            &amount_in_after_fee,
            &invariant,
        ) else {
            return Ok(false);
        };
        let Some(new_balance_out) = balances[index_out].checked_sub(amount_out_scaled) else {
            return Ok(false);
        };
        let new_balance_in = balances[index_in] + amount_in_after_fee;

        let min_balance = balances
            .iter()
            .copied()
            .min()
            .unwrap_or_default()
            .min(new_balance_out);
        let max_balance = balances
            .iter()
            .copied()
            .max()
            .unwrap_or_default()
            .max(new_balance_in);
        if min_balance.is_zero() {
            return Ok(false);
        }
        Ok(max_balance < STABLE_MAX_IMBALANCE_RATIO * min_balance)
    }

    /// Applies a completed swap to the live balances, mirroring what the Vault does on-chain.
    ///
    /// The protocol's share of the swap fee leaves the pool, so it is deducted from the input
    /// increment. `amount_out` is re-scaled from the raw amount the swap returned, which can land
    /// one unit below the value the Vault used internally; the resulting balance is short by at
    /// most that unit, which only matters for a route that crosses the same pool twice.
    fn with_swap_applied(
        &self,
        amount_in: U256,
        amount_out: U256,
        index_in: usize,
        index_out: usize,
    ) -> Result<Self, SimulationError> {
        let base = self.state.base();
        let maths_error = |e: balancer_maths_rust::PoolError| {
            SimulationError::FatalError(format!("balancer_v3 balance update failed: {e:?}"))
        };

        let amount_in_scaled = to_scaled_18_apply_rate_round_down(
            &amount_in,
            &base.scaling_factors[index_in],
            &base.token_rates[index_in],
        )
        .map_err(maths_error)?;
        let amount_out_scaled = to_scaled_18_apply_rate_round_down(
            &amount_out,
            &base.scaling_factors[index_out],
            &base.token_rates[index_out],
        )
        .map_err(maths_error)?;
        let total_fee_scaled =
            mul_up_fixed(&amount_in_scaled, &base.swap_fee).map_err(maths_error)?;
        // The Vault charges the protocol's share in the input token's own units, truncating the
        // fee to whole ones before deducting it from the raw balance. That is what this returns —
        // despite the scaled-18 name the maths library gives it — so it has to be scaled back up
        // before meeting balances that are held scaled to 18 decimals. Skipping that step leaves
        // the deduction short by the token's scaling factor, which is invisible for 18-decimal
        // tokens at a rate of one and a factor of 10^12 for something like USDC.
        let protocol_fee_raw = compute_and_charge_aggregate_swap_fees(
            &total_fee_scaled,
            &base.aggregate_swap_fee,
            &base.scaling_factors,
            &base.token_rates,
            index_in,
        )
        .map_err(maths_error)?;
        let protocol_fee_scaled = to_scaled_18_apply_rate_round_down(
            &protocol_fee_raw,
            &base.scaling_factors[index_in],
            &base.token_rates[index_in],
        )
        .map_err(maths_error)?;

        let mut balances = base.balances_live_scaled_18.clone();
        balances[index_in] += amount_in_scaled - protocol_fee_scaled;
        balances[index_out] = balances[index_out].saturating_sub(amount_out_scaled);

        let mut updated = self.clone();
        updated.set_balances(balances);
        Ok(updated)
    }

    fn set_balances(&mut self, balances: Vec<U256>) {
        match &mut self.state {
            PoolState::Weighted(state) => state.base.balances_live_scaled_18 = balances,
            PoolState::Stable(state) => state.base.balances_live_scaled_18 = balances,
            PoolState::ReClammV2(state) => state.base.balances_live_scaled_18 = balances,
            PoolState::QuantAmm(state) => state.base.balances_live_scaled_18 = balances,
            _ => {}
        }
    }
}

#[typetag::serde]
impl ProtocolSim for BalancerV3State {
    fn fee(&self) -> f64 {
        u256_to_f64(self.state.base().swap_fee)
            .map(|fee| fee / WAD)
            .unwrap_or(0.0)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let index_in = self.token_index(&base.address)?;
        let index_out = self.token_index(&quote.address)?;
        let pool_base = self.state.base();
        let balances = &pool_base.balances_live_scaled_18;

        // Probe a negligible fraction of the pool so the result approximates the marginal price.
        // `on_swap` is called before the Vault takes the swap fee, so the ratio is pre-fee and the
        // house-wide fee markup can be applied on top.
        let probe = (balances[index_in] / U256::from(SPOT_PRICE_PROBE_DIVISOR)).max(U256::from(1));
        let probe_failed = |e: PoolError| {
            SimulationError::RecoverableError(format!(
                "balancer_v3 spot price probe failed for pool {}: {e:?}",
                self.pool_address
            ))
        };
        let out = self
            .pool_impl()
            .map_err(probe_failed)?
            .on_swap(&SwapParams {
                swap_kind: SwapKind::GivenIn,
                token_in_index: index_in,
                token_out_index: index_out,
                amount_scaled_18: probe,
                balances_live_scaled_18: balances.clone(),
            })
            .map_err(probe_failed)?;

        // Live balances are already normalized to 18 decimals, so the decimal correction cancels;
        // what remains is undoing the token rates that scaled them into underlying-value terms.
        let ratio = u256_to_f64(out)? / u256_to_f64(probe)?;
        let rate_in = u256_to_f64(pool_base.token_rates[index_in])?;
        let rate_out = u256_to_f64(pool_base.token_rates[index_out])?;
        if rate_out == 0.0 {
            return Err(SimulationError::RecoverableError(format!(
                "balancer_v3 pool {} reports a zero rate for {}",
                self.pool_address, quote.address
            )));
        }
        Ok(add_fee_markup(ratio * rate_in / rate_out, self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let index_in = self.token_index(&token_in.address)?;
        let index_out = self.token_index(&token_out.address)?;
        let amount_in = biguint_to_u256(&amount_in);
        let amount_out = self.swap_exact_in(amount_in, &token_in.address, &token_out.address)?;
        let new_state = self.with_swap_applied(amount_in, amount_out, index_in, index_out)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            SWAP_GAS
                .to_biguint()
                .expect("u64 fits in BigUint"),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let index_in = self.token_index(&sell_token)?;
        let index_out = self.token_index(&buy_token)?;
        let base = self.state.base();
        if base.balances_live_scaled_18[index_in].is_zero() ||
            base.balances_live_scaled_18[index_out].is_zero()
        {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }

        let max_in = self.max_swap_amount_in(index_in, index_out)?;
        if max_in.is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        // A pool so close to empty that even its own largest swap trades below the Vault's
        // minimum is a dust pool with nothing to quote, not a fatal error.
        let max_out = match self.vault_swap_exact_in(max_in, &sell_token, &buy_token) {
            Ok(amount_out) => amount_out,
            Err(PoolError::TradeAmountTooSmall) => return Ok((BigUint::ZERO, BigUint::ZERO)),
            Err(e) => {
                return Err(SimulationError::RecoverableError(format!(
                    "balancer_v3 swap failed for pool {}: {e:?}",
                    self.pool_address
                )))
            }
        };
        Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // The decoder attaches the current block's timestamp to every delta. reCLAMM quotes move
        // with it, so take it when present and keep the previous one otherwise.
        if let Some(timestamp) = delta
            .updated_attributes
            .get(BLOCK_TIMESTAMP_ATTRIBUTE)
            .and_then(|raw| raw.as_ref().try_into().ok())
            .map(u64::from_be_bytes)
        {
            self.block_timestamp = timestamp;
        }

        let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");
        let pool = AlloyAddress::from_slice(self.pool_address.as_ref());
        let vault = AlloyAddress::from_slice(self.vault.as_ref());
        self.state = vm::refresh_pool_state(
            &engine,
            &pool,
            &vault,
            &self.state,
            self.pool_type,
            self.block_timestamp,
        )
        .map_err(TransitionError::SimulationError)?;
        Ok(())
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
            .downcast_ref::<Self>()
            .is_some_and(|other| self == other)
    }
}

/// `WeightedMath.computeInGivenExactOut` without its own `MAX_OUT_RATIO` guard.
///
/// That guard exists to bound real exact-out swaps; here the formula is only used to invert
/// `computeOutGivenExactIn` and find the input a min-balance-derived output ceiling implies, a use
/// the ratio was never meant to constrain — a heavily skewed weighted pool can legitimately move
/// more than 30% of the output reserve on an exact-in swap that only spends a small share of the
/// input reserve.
fn weighted_in_given_exact_out_unguarded(
    balance_in: &U256,
    weight_in: &U256,
    balance_out: &U256,
    weight_out: &U256,
    amount_out: &U256,
) -> Result<U256, PoolError> {
    let base = div_up_fixed(balance_out, &(balance_out - amount_out))?;
    let exponent = div_up_fixed(weight_out, weight_in)?;
    let power = pow_up_fixed(&base, &exponent)?;
    let ratio = power - ONE_WAD_SCALED_18;
    mul_up_fixed(balance_in, &ratio)
}

/// Converts a live scaled-18 balance back into the token's raw units.
#[cfg(test)]
fn raw_balance(
    base: &balancer_maths_rust::common::types::BasePoolState,
    index: usize,
) -> Result<U256, SimulationError> {
    to_raw_undo_rate_round_down(
        &base.balances_live_scaled_18[index],
        &base.scaling_factors[index],
        &base.token_rates[index],
    )
    .map_err(|e| SimulationError::FatalError(format!("balancer_v3 balance rescale failed: {e:?}")))
}
