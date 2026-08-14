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
            compute_and_charge_aggregate_swap_fees_raw, to_raw_undo_rate_round_down,
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
        balancer_v3::vm,
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
/// The storage-derived parts of the state are re-read from the VM on every
/// [`ProtocolSim::delta_transition`] rather than patched from the delta: the values the maths
/// needs (live balances, token rates, amplification) are derived from storage that other
/// contracts — rate providers above all — own. What registration fixed forever (tokens, scaling
/// factors, weights, the hook check) is kept from decode time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BalancerV3State {
    /// Pool contract address (the Tycho component id).
    pool_address: Bytes,
    /// Vault the pool is registered with, kept so updates need no extra lookup.
    vault: Bytes,
    /// Token addresses in pool registration order, which the maths library indexes balances, rates
    /// and weights by.
    tokens: Vec<Bytes>,
    /// Per-token minimum live balance (scaled 18, registration order) a weighted pool's own
    /// `MinTokenBalanceLib` check enforces. Empty when no such floor applies: non-weighted pools,
    /// and weighted generations predating the check (`getMinTokenBalances` reverts on them).
    min_token_balances: Vec<U256>,
    /// Timestamp of the block this state was read at. reCLAMM quotes depend on it, so it is
    /// refreshed on every update; the other families ignore it.
    block_timestamp: u64,
    /// Pool state in the form the maths library consumes.
    state: PoolState,
}

impl BalancerV3State {
    pub(super) fn new(
        pool_address: Bytes,
        vault: Bytes,
        tokens: Vec<Bytes>,
        min_token_balances: Vec<U256>,
        block_timestamp: u64,
        state: PoolState,
    ) -> Self {
        Self { pool_address, vault, tokens, min_token_balances, block_timestamp, state }
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

    /// Builds the pool implementation the maths dispatches a swap to, which `Vault::swap` would
    /// otherwise build itself. [`Self::spot_price`] needs it for a second reason: only `on_swap`
    /// reports an amount before the swap fee is taken.
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
            token_in: format!("0x{}", hex::encode(token_in)),
            token_out: format!("0x{}", hex::encode(token_out)),
        };
        // `Vault::swap` takes the state by `Box<PoolState>`, cloning it on every call — which the
        // limit searches make hundreds of. Supplying the pool implementation and hook directly is
        // all its body does before delegating here, and lets the state be borrowed. The no-op hook
        // is faithful because the decoder rejects pools carrying a swap hook.
        vault_swap(&input, &self.state, self.pool_impl()?.as_ref(), &DefaultHook::new(), None)
    }

    /// Largest input the Vault accepts for a swap from `index_in` to `index_out`, in the input
    /// token's raw units.
    ///
    /// Mirrors the reference implementation's `getMaxSwapAmount` for exact-in swaps. Each family
    /// bounds it differently — see the per-family functions below; reCLAMM is bounded on the
    /// output side, by the input that buys [`MAX_TOKEN_OUT_RATIO`] of the output reserve at the
    /// current virtual balances.
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
                // A pool whose reserves and virtual balances are small enough for its invariant to
                // round to zero has no price range to speak of, and reports as much rather than
                // dividing by it. Recoverable: a re-seeded pool quotes again.
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
                )
                .map_err(|e| {
                    SimulationError::RecoverableError(format!(
                        "balancer_v3 reCLAMM pool {} has no usable price range: {e:?}",
                        self.pool_address
                    ))
                })?;
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
    /// `onSwap` applies `maxTradeSizeRatio` to *both* sides, so the input reserve's share of it is
    /// only an upper bound — on a pool whose weights are far apart, a permitted input can still
    /// buy more of the output reserve than the same ratio allows. Inverting the output bound in
    /// closed form would mean recomputing the pool's time-interpolated weights here, which would
    /// drift from the quotes `balancer_maths_rust` produces; probing the real swap keeps the limit
    /// consistent with [`Self::get_amount_out`] by construction. The predicate is monotonic in the
    /// input, so the search converges on the largest accepted amount, or zero if there is none.
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
    /// generation and not modelled by `balancer_maths_rust`) also require that neither balance
    /// fall below its minimum after the swap, inverted here through
    /// [`weighted_in_given_exact_out_unguarded`]. The tighter of the two wins.
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

        let min_balance_cap = match weighted_in_given_exact_out_unguarded(
            &balances[index_in],
            &weights[index_in],
            &balances[index_out],
            &weights[index_out],
            &target_out,
        ) {
            Ok(cap) => cap,
            // The inversion raises the output reserve's depletion ratio to `weight_out /
            // weight_in`, which is 99 on a 99/1 pool. Overflowing it means buying the output side
            // down to its floor would take more input than a `U256` holds, so the minimum sits
            // far beyond `MAX_IN_RATIO` and cannot be what binds.
            Err(PoolError::MathOverflow) => return Ok(ratio_cap),
            Err(e) => return Err(maths_error(e)),
        };
        Ok(ratio_cap.min(min_balance_cap))
    }

    /// Largest exact-in input a stable-pool swap can take without the live balances drifting past
    /// [`STABLE_MAX_IMBALANCE_RATIO`]. That check has no closed-form inverse over the stable
    /// invariant, but [`Self::stable_swap_keeps_balance_valid`] is monotonic in the input, so the
    /// binary search converges to within a wei.
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

        // `stable_math::compute_invariant` divides by each balance unchecked, so a zero balance on
        // an untouched token would panic rather than error — reachable in a pool with more than
        // two tokens, since `get_limits` only screens `index_in` and `index_out`.
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
    /// increment. Re-scaling `amount_out` from the raw amount can land one unit below the value
    /// the Vault used, which only matters for a route crossing the same pool twice.
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
        // This returns the fee in the input token's raw units, truncated to whole ones, so it has
        // to be scaled back up before meeting balances held at 18 decimals. Skipping that leaves
        // the deduction short by the token's scaling factor — 10^12 for something like USDC.
        let protocol_fee_raw = compute_and_charge_aggregate_swap_fees_raw(
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
        let amount_out = self
            .vault_swap_exact_in(amount_in, &token_in.address, &token_out.address)
            .map_err(|e| {
                SimulationError::RecoverableError(format!(
                    "balancer_v3 swap failed for pool {}: {e:?}",
                    self.pool_address
                ))
            })?;
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
        self.state =
            vm::refresh_pool_state(&engine, &pool, &vault, &self.state, self.block_timestamp)
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
/// That guard bounds real exact-out swaps; here the formula only inverts `computeOutGivenExactIn`
/// to find the input a min-balance-derived output ceiling implies. A heavily skewed pool can
/// legitimately move more than 30% of the output reserve on an exact-in swap.
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
