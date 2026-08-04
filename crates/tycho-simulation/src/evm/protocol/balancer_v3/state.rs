//! [`BalancerV3State`] — a hybrid Balancer V3 pool: pure-Rust quote maths
//! (`balancer_maths_rust`) over state read from the locally indexed VM storage.
use std::any::Any;

use alloy::primitives::{Address as AlloyAddress, U256};
use balancer_maths_rust::{
    common::{
        maths::mul_up_fixed,
        pool_base::PoolBase,
        types::{PoolState, PoolStateOrBuffer, SwapInput, SwapKind, SwapParams},
        utils::{compute_and_charge_aggregate_swap_fees, to_scaled_18_apply_rate_round_down},
    },
    pools::{reclammv2::ReClammV2Pool, stable::StablePool, weighted::WeightedPool},
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
        balancer_v3::vm::{self, BalancerPoolType},
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
/// How many times [`ProtocolSim::get_limits`] halves its candidate before giving up.
const LIMIT_PROBE_HALVINGS: u32 = 12;

/// A single Balancer V3 pool quoted through `balancer_maths_rust`.
///
/// `tokens` follows the pool's own registration order, which is what the maths library indexes
/// balances, rates and weights by. State is rebuilt from the VM on every
/// [`ProtocolSim::delta_transition`] rather than patched from the delta, because the values the
/// maths needs (live balances, token rates, amplification) are derived from storage that other
/// contracts — rate providers above all — own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BalancerV3State {
    /// Pool contract address (the Tycho component id).
    pool_address: Bytes,
    /// Vault the pool is registered with, kept so updates need no extra lookup.
    vault: Bytes,
    /// Resolved pool family.
    pool_type: BalancerPoolType,
    /// Token addresses in pool registration order.
    tokens: Vec<Bytes>,
    /// Timestamp of the block this state was read at. reCLAMM quotes depend on it, so it is
    /// refreshed on every update; the other families ignore it.
    block_timestamp: u64,
    /// Pool state in the form the maths library consumes.
    state: PoolState,
}

impl BalancerV3State {
    /// Constructs a state from a resolved pool type and a state read out of the VM.
    pub(super) fn new(
        pool_address: Bytes,
        vault: Bytes,
        pool_type: BalancerPoolType,
        tokens: Vec<Bytes>,
        block_timestamp: u64,
        state: PoolState,
    ) -> Self {
        Self { pool_address, vault, pool_type, tokens, block_timestamp, state }
    }

    /// Token addresses in pool registration order.
    #[cfg(test)]
    pub(super) fn token_addresses(&self) -> &[Bytes] {
        &self.tokens
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

        // Start from the pool's own balance of the sell token, expressed raw, and halve until the
        // maths accepts the size. Weighted pools reject anything above `MAX_IN_RATIO` (30% of the
        // balance) outright, and stable pools stop solving once the input dwarfs the reserve, so
        // the first accepted candidate is the usable soft limit.
        let mut candidate = raw_balance(base, index_in)?;
        for _ in 0..LIMIT_PROBE_HALVINGS {
            if candidate.is_zero() {
                break;
            }
            if let Ok(amount_out) = self.swap_exact_in(candidate, &sell_token, &buy_token) {
                if !amount_out.is_zero() {
                    return Ok((u256_to_biguint(candidate), u256_to_biguint(amount_out)));
                }
            }
            candidate /= U256::from(2);
        }
        Ok((BigUint::ZERO, BigUint::ZERO))
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
            vm::read_pool_state(&engine, &pool, &vault, self.pool_type, self.block_timestamp)
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
fn raw_balance(
    base: &balancer_maths_rust::common::types::BasePoolState,
    index: usize,
) -> Result<U256, SimulationError> {
    balancer_maths_rust::common::utils::to_raw_undo_rate_round_down(
        &base.balances_live_scaled_18[index],
        &base.scaling_factors[index],
        &base.token_rates[index],
    )
    .map_err(|e| SimulationError::FatalError(format!("balancer_v3 balance rescale failed: {e:?}")))
}
