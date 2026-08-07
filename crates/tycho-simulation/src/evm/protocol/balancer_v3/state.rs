//! [`BalancerV3State`] — a hybrid Balancer V3 pool: pure-Rust quote maths
//! (`balancer_maths_rust`) over state read from the locally indexed VM storage.
use std::any::Any;

use alloy::primitives::{Address as AlloyAddress, U256};
use balancer_maths_rust::{
    common::{
        maths::{mul_down_fixed, mul_up_fixed},
        pool_base::PoolBase,
        types::{PoolState, PoolStateOrBuffer, SwapInput, SwapKind, SwapParams},
        utils::{
            compute_and_charge_aggregate_swap_fees, to_raw_undo_rate_round_down,
            to_scaled_18_apply_rate_round_down,
        },
    },
    pools::{
        reclammv2::{compute_current_virtual_balances, compute_in_given_out, ReClammV2Pool},
        stable::StablePool,
        weighted::{WeightedPool, MAX_IN_RATIO},
    },
    vault::Vault,
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
    /// Timestamp of the block this state was read at. reCLAMM quotes depend on it, so it is
    /// refreshed on every update; the other families ignore it.
    block_timestamp: u64,
    /// Pool state in the form the maths library consumes.
    state: PoolState,
}

impl BalancerV3State {
    /// Constructs a state from a resolved `pool_type` attribute and a state read out of the VM.
    pub(super) fn new(
        pool_address: Bytes,
        vault: Bytes,
        factory: PoolTypeAttribute,
        tokens: Vec<Bytes>,
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

    /// Builds the pool implementation used for pre-fee probing.
    ///
    /// The `Vault` builds this internally for swaps; [`Self::spot_price`] needs it directly because
    /// only `on_swap` reports an amount before the swap fee is taken.
    fn pool_impl(&self) -> Result<Box<dyn PoolBase>, SimulationError> {
        match &self.state {
            PoolState::Weighted(state) => Ok(Box::new(WeightedPool::from(state.clone()))),
            PoolState::Stable(state) => Ok(Box::new(StablePool::new(state.mutable.clone()))),
            PoolState::ReClammV2(state) => Ok(Box::new(ReClammV2Pool::new(state.clone()))),
            other => Err(SimulationError::FatalError(format!(
                "balancer_v3 pool {} holds unsupported state `{}`",
                self.pool_address,
                other.pool_type()
            ))),
        }
    }

    /// Swaps `amount_in` of `token_in` for `token_out`, returning the raw output amount.
    fn swap_exact_in(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<U256, SimulationError> {
        let input = SwapInput {
            amount_raw: amount_in,
            swap_kind: SwapKind::GivenIn,
            token_in: Self::token_key(token_in),
            token_out: Self::token_key(token_out),
        };
        Vault::new()
            .swap(&input, &PoolStateOrBuffer::Pool(Box::new(self.state.clone())), None)
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
    /// pools cap the input at [`MAX_IN_RATIO`] of the input reserve, which `WeightedMath`
    /// enforces on chain. Stable maths accepts any input the Vault can still store, so the cap
    /// is the balance headroom up to [`MAX_VAULT_BALANCE`]. reCLAMM pools are bounded by the
    /// output side: the limit is the input that buys [`MAX_TOKEN_OUT_RATIO`] of the output
    /// reserve at the current virtual balances.
    fn max_swap_amount_in(
        &self,
        index_in: usize,
        index_out: usize,
    ) -> Result<U256, SimulationError> {
        let base = self.state.base();
        let balances = &base.balances_live_scaled_18;
        let maths_error = |e: balancer_maths_rust::PoolError| {
            SimulationError::FatalError(format!(
                "balancer_v3 swap limit failed for pool {}: {e:?}",
                self.pool_address
            ))
        };

        let max_in_scaled_18 = match &self.state {
            PoolState::Weighted(_) => {
                mul_down_fixed(&balances[index_in], &MAX_IN_RATIO).map_err(maths_error)?
            }
            PoolState::Stable(_) => MAX_VAULT_BALANCE.saturating_sub(balances[index_in]),
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
        let protocol_fee_scaled = compute_and_charge_aggregate_swap_fees(
            &total_fee_scaled,
            &base.aggregate_swap_fee,
            &base.scaling_factors,
            &base.token_rates,
            index_in,
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
        let out = self
            .pool_impl()?
            .on_swap(&SwapParams {
                swap_kind: SwapKind::GivenIn,
                token_in_index: index_in,
                token_out_index: index_out,
                amount_scaled_18: probe,
                balances_live_scaled_18: balances.clone(),
            })
            .map_err(|e| {
                SimulationError::RecoverableError(format!(
                    "balancer_v3 spot price probe failed for pool {}: {e:?}",
                    self.pool_address
                ))
            })?;

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
        let max_out = self.swap_exact_in(max_in, &sell_token, &buy_token)?;
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
