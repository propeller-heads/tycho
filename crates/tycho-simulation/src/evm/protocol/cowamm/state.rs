use std::{any::Any, collections::HashMap, fmt::Debug};

use alloy::primitives::U256;
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

use crate::evm::protocol::{
    cowamm::{
        bmath::*,
        constants::{BONE, MAX_IN_RATIO},
        error::CowAMMError,
    },
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
};

const COWAMM_FEE: f64 = 0.0; // 0% fee

// Token 3 tuple: (address, liquidity, weight)
type TokenInfo = (Bytes, U256, U256);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CowAMMState {
    /// The Pool Address
    pub address: Bytes,
    /// Token A information: (address, liquidity, weight)
    pub token_a: TokenInfo,
    /// Token B information: (address, liquidity, weight)
    pub token_b: TokenInfo,
    /// The Swap Fee on the Pool
    pub fee: u64,
    ///The lp token of the pool
    pub lp_token: Bytes,
    /// The Supply of the lp token
    pub lp_token_supply: U256,
}

impl CowAMMState {
    /// Creates a new `CowAMMState` instance.
    ///
    /// # Arguments
    /// - `token_a`: The address of the first token in the pair.
    /// - `token_b`: The address of the second token in the pair.
    /// - `liquidity_a`: Liquidity of the first token in the pair.
    /// - `liquidity_b`: Liquidity of the second token in the pair.
    /// - `lp_token`: The pool address (usually the LP token address).
    /// - `lp_token_supply`: The supply of the lp_token in the pool
    /// - `weight_a`: The denormalized weight of `token_a`.
    /// - `weight_b`: The denormalized weight of `token_b`.
    /// - `fee`: The swap fee for the pool.
    ///
    /// # Returns
    /// A new `CowAMMState` with token states initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Bytes,
        token_a_addr: Bytes,
        token_b_addr: Bytes,
        liquidity_a: U256,
        liquidity_b: U256,
        lp_token: Bytes,
        lp_token_supply: U256,
        weight_a: U256,
        weight_b: U256,
        fee: u64,
    ) -> Self {
        Self {
            address,
            token_a: (token_a_addr, liquidity_a, weight_a),
            token_b: (token_b_addr, liquidity_b, weight_b),
            lp_token,
            lp_token_supply,
            fee,
        }
    }
    /// Helper methods
    fn token_a_addr(&self) -> &Bytes {
        &self.token_a.0
    }

    fn liquidity_a(&self) -> U256 {
        self.token_a.1
    }

    fn liquidity_b(&self) -> U256 {
        self.token_b.1
    }

    fn weight_a(&self) -> U256 {
        self.token_a.2
    }

    fn weight_b(&self) -> U256 {
        self.token_b.2
    }

    /// Calculates the proportion of tokens a user receives when exiting the pool by burning LP
    /// tokens,
    ///
    ///
    /// Solidity reference:
    /// https://github.com/balancer/balancer-v2-monorepo/blob/master/pkg/core/contracts/pools/weighted/WeightedMath.sol#L299
    ///
    /// Formula:
    /// amountOut\[i\] = balances\[i\] * (lpTokenAmountIn / totalLpToken)
    ///
    /// # Arguments
    /// * `pool` - Reference to the pool containing balances and LP supply
    /// * `lp_token_in` - Amount of LP tokens to burn
    ///
    /// # Returns
    /// Tuple of `(amountOut_tokenA, amountOut_tokenB)`
    pub fn calc_tokens_out_given_exact_lp_token_in(
        &self,
        lp_token_in: U256,
    ) -> Result<(U256, U256), SimulationError> {
        // Collect balances
        let liquidity_a = self.liquidity_a();
        let liquidity_b = self.liquidity_b();

        let balances = [liquidity_a, liquidity_b];

        let total_lp_token = self.lp_token_supply;

        // lpTokenRatio = lp_token_in / total_lp_token
        let lp_token_ratio = bdiv(lp_token_in, total_lp_token).map_err(|err| {
            SimulationError::FatalError(format!("Error in calculating LP token ratio {err:?}"))
        })?;

        // amountsOut[i] = balances[i] * lp_token_ratio
        let mut amounts_out = Vec::with_capacity(balances.len());

        for balance in balances.iter() {
            let amount = bmul(*balance, lp_token_ratio).map_err(|err| {
                SimulationError::FatalError(format!("Error in calculating amount out {err:?}"))
            })?;
            amounts_out.push(amount);
        }
        Ok((amounts_out[0], amounts_out[1]))
    }

    //https://github.com/balancer/cow-amm/blob/main/src/contracts/BPool.sol#L174
    /// joinPool
    pub fn join_pool(
        &self,
        new_state: &mut CowAMMState,
        pool_amount_out: U256,
        max_amounts_in: &[U256],
    ) -> Result<(), CowAMMError> {
        let pool_total = new_state.lp_token_supply;
        let ratio = bdiv(pool_amount_out, pool_total)?;

        if ratio.is_zero() {
            return Err(CowAMMError::InvalidPoolRatio);
        }

        let balances = vec![new_state.liquidity_a(), new_state.liquidity_b()];

        for (i, bal) in balances.into_iter().enumerate() {
            let token_amount_in = bmul(ratio, bal)?;
            if token_amount_in.is_zero() {
                return Err(CowAMMError::InvalidTokenAmountIn);
            }
            if token_amount_in > max_amounts_in[i] {
                return Err(CowAMMError::TokenAmountInAboveMax);
            }

            // equivalent to _pullUnderlying
            if i == 0 {
                new_state.token_a.1 = badd(new_state.token_a.1, token_amount_in)?;
            } else {
                new_state.token_b.1 = badd(new_state.token_b.1, token_amount_in)?;
            }
        }

        // mint LP shares
        new_state.lp_token_supply = badd(new_state.lp_token_supply, pool_amount_out)?;
        Ok(())
    }

    /// exitPool
    pub fn exit_pool(
        &self,
        new_state: &mut CowAMMState,
        pool_amount_in: U256,
        min_amounts_out: &[U256],
        exit_fee: U256,
    ) -> Result<(), CowAMMError> {
        let pool_total = self.lp_token_supply;

        // calculate fee
        let fee = bmul(pool_amount_in, exit_fee)?;

        let pai_after_fee = bsub(pool_amount_in, fee)?;
        let ratio = bdiv(pai_after_fee, pool_total)?;

        if ratio.is_zero() {
            return Err(CowAMMError::InvalidPoolRatio);
        }

        // burn LP shares
        new_state.lp_token_supply = bsub(self.lp_token_supply, pai_after_fee)?;

        let balances = vec![self.liquidity_a(), self.liquidity_b()];
        for (i, bal) in balances.into_iter().enumerate() {
            let token_amount_out = bmul(ratio, bal)?;

            if token_amount_out.is_zero() {
                return Err(CowAMMError::InvalidTokenAmountOut);
            }

            if token_amount_out < min_amounts_out[i] {
                return Err(CowAMMError::TokenAmountOutBelowMinAmountOut);
            }

            // Update new_state balances
            if i == 0 {
                new_state.token_a.1 = bsub(self.token_a.1, token_amount_out)?;
            } else {
                new_state.token_b.1 = bsub(self.token_b.1, token_amount_out)?;
            }
        }

        Ok(())
    }

    /// Calculates swap limits for operations involving LP tokens.
    ///
    /// # Parameters
    /// - `is_lp_buy`:
    ///   - `false` → **LP → Token** (redeeming LP tokens)
    ///   - `true`  → **Token → LP** (minting or acquiring LP tokens)
    /// - `sell_token`: Token being provided by the user
    /// - `buy_token`: Token being received by the user
    ///
    /// # Returns
    /// - When `is_lp_buy == false` (**LP → Token**):
    ///   - `(max_lp_in, max_token_out)`
    ///     - `max_lp_in`: Maximum LP tokens that can be redeemed without exceeding swap limits
    ///     - `max_token_out`: Maximum amount of the underlying token that can be received
    ///
    /// - When `is_lp_buy == true` (**Token → LP**):
    ///   - `(max_token_in, max_lp_out)`
    ///     - `max_token_in`: Maximum amount of the underlying token that can be provided
    ///     - `max_lp_out`: Maximum LP tokens that can be minted or received
    fn get_lp_swap_limits(
        &self,
        is_lp_buy: bool,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        // LP swaps are modeled as: swap part of the input to the other leg, then join/exit.
        // Limits cap the intermediate swap via MAX_IN_RATIO; dust is allowed.
        if is_lp_buy {
            // Buy LP: swap part of the sell token into the other leg, then join.
            let is_token_a_in = sell_token == *self.token_a_addr();

            let (bal_in, weight_in, bal_out, weight_out) = if is_token_a_in {
                (self.liquidity_a(), self.weight_a(), self.liquidity_b(), self.weight_b())
            } else {
                (self.liquidity_b(), self.weight_b(), self.liquidity_a(), self.weight_a())
            };

            // Cap total input by MAX_IN_RATIO of the input token balance.
            let max_token_in = bmul(bal_in, MAX_IN_RATIO)
                .map_err(|err| SimulationError::FatalError(format!("max_in error: {err:?}")))?;

            if max_token_in.is_zero() || bal_in.is_zero() || bal_out.is_zero() {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }

            // Find a split (x to swap, max_token_in - x to keep) that matches pool proportions.
            let mut lo = U256::ZERO;
            let mut hi = max_token_in;
            let mut best_x = U256::ZERO;
            for _ in 0..128 {
                let x = safe_div_u256(safe_add_u256(lo, hi)?, U256::from(2u8))?;
                let out = calculate_out_given_in(
                    bal_in,
                    weight_in,
                    bal_out,
                    weight_out,
                    x,
                    U256::from(self.fee),
                )
                .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

                let in_remaining = safe_sub_u256(max_token_in, x)?;
                let bal_in_after = safe_add_u256(bal_in, x)?;
                let bal_out_after = safe_sub_u256(bal_out, out)?;

                // Target: in_remaining / bal_in_after == out / bal_out_after
                let left = safe_mul_u256(in_remaining, bal_out_after)?;
                let right = safe_mul_u256(out, bal_in_after)?;

                if left > right {
                    lo = safe_add_u256(x, U256::from(1u8))?;
                } else {
                    best_x = x;
                    if x.is_zero() {
                        break;
                    }
                    hi = safe_sub_u256(x, U256::from(1u8))?;
                }
            }

            let x = best_x;
            let out = calculate_out_given_in(
                bal_in,
                weight_in,
                bal_out,
                weight_out,
                x,
                U256::from(self.fee),
            )
            .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

            let in_remaining = safe_sub_u256(max_token_in, x)?;
            let bal_in_after = safe_add_u256(bal_in, x)?;
            let bal_out_after = safe_sub_u256(bal_out, out)?;

            // LP minting is limited by the smaller proportional contribution.
            let pool_total = self.lp_token_supply;
            let lp_from_in = safe_div_u256(safe_mul_u256(in_remaining, pool_total)?, bal_in_after)?;
            let lp_from_out = safe_div_u256(safe_mul_u256(out, pool_total)?, bal_out_after)?;
            let max_lp_out = if lp_from_in < lp_from_out { lp_from_in } else { lp_from_out };

            Ok((u256_to_biguint(max_token_in), u256_to_biguint(max_lp_out)))
        } else {
            // Sell LP: exit to both tokens, then swap the unwanted leg into the desired token.
            let is_token_a_out = buy_token == *self.token_a_addr();

            let (unwanted_liquidity, unwanted_weight, wanted_liquidity, wanted_weight) =
                if is_token_a_out {
                    // Want token_a; swap token_b into token_a after exit.
                    (self.liquidity_b(), self.weight_b(), self.liquidity_a(), self.weight_a())
                } else {
                    // Want token_b; swap token_a into token_b after exit.
                    (self.liquidity_a(), self.weight_a(), self.liquidity_b(), self.weight_b())
                };

            if unwanted_liquidity.is_zero() {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }

            // Cap the exit ratio so the unwanted leg can be fully swapped under MAX_IN_RATIO.
            let max_intermediate_swap_in = bmul(unwanted_liquidity, MAX_IN_RATIO)
                .map_err(|err| SimulationError::FatalError(format!("max_in error: {err:?}")))?;

            let ratio = bdiv(max_intermediate_swap_in, unwanted_liquidity)
                .map_err(|err| SimulationError::FatalError(format!("ratio error: {err:?}")))?;

            // LP in upper bound implied by the ratio cap.
            let max_lp_in = bmul(self.lp_token_supply, ratio)
                .map_err(|err| SimulationError::FatalError(format!("max_lp_in error: {err:?}")))?;

            // Wanted token from the exit leg at the capped ratio.
            let exit_wanted = bmul(wanted_liquidity, ratio).map_err(|err| {
                SimulationError::FatalError(format!("exit_wanted error: {err:?}"))
            })?;

            // Wanted token from swapping the unwanted leg at MAX_IN_RATIO.
            let swap_out = calculate_out_given_in(
                unwanted_liquidity,
                unwanted_weight,
                wanted_liquidity,
                wanted_weight,
                max_intermediate_swap_in,
                U256::from(self.fee),
            )
            .map_err(|err| SimulationError::FatalError(format!("amount_out error: {err:?}")))?;

            // Total upper bound: wanted from exit + wanted from swap.
            let max_token_out = safe_add_u256(exit_wanted, swap_out)?;

            Ok((u256_to_biguint(max_lp_in), u256_to_biguint(max_token_out)))
        }
    }
}

#[typetag::serde]
impl ProtocolSim for CowAMMState {
    fn fee(&self) -> f64 {
        COWAMM_FEE
    }
    /// Calculates a f64 representation of base token price in the AMM.
    /// ********************************************************************************************
    /// ** calcSpotPrice
    /// // sP = spotPrice
    /// // bI = tokenBalanceIn                ( bI / wI )         1
    /// // bO = tokenBalanceOut         sP =  -----------  *  ----------
    /// // wI = tokenWeightIn                 ( bO / wO )     ( 1 - sF )
    /// // wO = tokenWeightOut
    /// // sF = swapFee
    /// // *************************************************************************************
    /// *********/
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let (bal_in, weight_in) = if base.address == *self.token_a_addr() {
            (self.liquidity_a(), self.weight_a())
        } else if base.address == self.token_b.0 {
            (self.liquidity_b(), self.weight_b())
        } else {
            return Err(SimulationError::FatalError(
                "spot_price base token not in pool".to_string(),
            ));
        };

        let (bal_out, weight_out) = if quote.address == *self.token_a_addr() {
            (self.liquidity_a(), self.weight_a())
        } else if quote.address == self.token_b.0 {
            (self.liquidity_b(), self.weight_b())
        } else {
            return Err(SimulationError::FatalError(
                "spot_price quote token not in pool".to_string(),
            ));
        };

        let numer = bdiv(bal_in, weight_in).map_err(|err| {
            SimulationError::FatalError(format!(
                "Error in numerator bdiv(balance_base / weight_base): {err:?}"
            ))
        })?;
        let denom = bdiv(bal_out, weight_out).map_err(|err| {
            SimulationError::FatalError(format!(
                "Error in denominator bdiv(balance_quote / weight_quote): {err:?}"
            ))
        })?;

        let ratio = bmul(
            bdiv(numer, denom).map_err(|err| {
                SimulationError::FatalError(format!("Error in (numer / denom): {err:?}"))
            })?,
            BONE,
        )
        .map_err(|err| {
            SimulationError::FatalError(format!("Error in bmul(ratio * scale): {err:?}"))
        })?;

        u256_to_f64(ratio)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        if amount_in.is_zero() {
            return Err(SimulationError::InvalidInput("Amount in cannot be zero".to_string(), None));
        }

        let is_lp_in = token_in.address == self.address;
        let is_lp_out = token_out.address == self.address;

        if is_lp_in && is_lp_out {
            return Err(SimulationError::InvalidInput(
                "Cannot swap LP token for LP token".into(),
                None,
            ));
        }

        let mut new_state = self.clone();

        // ============================
        // EXIT POOL (LP → TOKEN) //
        // ============================
        if is_lp_in && !is_lp_out {
            let (proportional_token_amount_a, proportional_token_amount_b) = self
                .calc_tokens_out_given_exact_lp_token_in(amount_in)
                .map_err(|e| {
                    SimulationError::FatalError(format!(
                        "failed to calculate token proportions out error: {e:?}"
                    ))
                })?;
            self.exit_pool(
                &mut new_state,
                amount_in,
                &[proportional_token_amount_a, proportional_token_amount_b],
                U256::from(self.fee),
            )
            .map_err(|err| SimulationError::FatalError(format!("exit_pool error: {err:?}")))?;

            let (amount_to_swap, is_token_a_swap_in) = if token_out.address == *self.token_a_addr()
            {
                (proportional_token_amount_b, false)
            } else {
                (proportional_token_amount_a, true)
            };

            let amount_out = if is_token_a_swap_in {
                calculate_out_given_in(
                    new_state.liquidity_a(),
                    new_state.weight_a(),
                    new_state.liquidity_b(),
                    new_state.weight_b(),
                    amount_to_swap,
                    U256::from(self.fee),
                )
            } else {
                calculate_out_given_in(
                    new_state.liquidity_b(),
                    new_state.weight_b(),
                    new_state.liquidity_a(),
                    new_state.weight_a(),
                    amount_to_swap,
                    U256::from(self.fee),
                )
            }
            .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

            if is_token_a_swap_in {
                new_state.token_a.1 = safe_sub_u256(new_state.liquidity_a(), amount_to_swap)?;
                new_state.token_b.1 = safe_add_u256(new_state.liquidity_b(), amount_out)?;
            } else {
                new_state.token_b.1 = safe_sub_u256(new_state.liquidity_b(), amount_to_swap)?;
                new_state.token_a.1 = safe_add_u256(new_state.liquidity_a(), amount_out)?;
            }

            let total_trade_amount = if is_token_a_swap_in {
                safe_add_u256(amount_out, proportional_token_amount_b)?
            } else {
                safe_add_u256(amount_out, proportional_token_amount_a)?
            };

            return Ok(GetAmountOutResult {
                amount: u256_to_biguint(total_trade_amount),
                gas: 194_140u64.to_biguint().unwrap(),
                new_state: Box::new(new_state),
            });
        }

        // ============================
        // JOIN POOL (TOKEN → LP)
        // ============================
        if is_lp_out && !is_lp_in {
            // This is the TOKEN -> LP flow: we will split the input token into
            // (1) a swap leg to balance pool proportions and (2) a join leg to mint LP.
            let fee = U256::from(self.fee);
            let (bal_in, weight_in, bal_out, weight_out, is_token_a_in) =
                if token_in.address == *new_state.token_a_addr() {
                    (
                        new_state.liquidity_a(),
                        new_state.weight_a(),
                        new_state.liquidity_b(),
                        new_state.weight_b(),
                        true,
                    )
                } else {
                    (
                        new_state.liquidity_b(),
                        new_state.weight_b(),
                        new_state.liquidity_a(),
                        new_state.weight_a(),
                        false,
                    )
                };

            // Binary search the swap amount (x) that makes the post-swap ratio
            // match the join ratio, minimizing dust on the join leg.
            let mut lo = U256::ZERO;
            let mut hi = amount_in;
            let mut best_x = U256::ZERO;
            for _ in 0..128 {
                // Midpoint in integer space (floor division).
                let x = safe_div_u256(safe_add_u256(lo, hi)?, U256::from(2u8))?;
                let out = calculate_out_given_in(bal_in, weight_in, bal_out, weight_out, x, fee)
                    .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

                // Split: x is swapped, the remainder joins as the original token.
                let in_remaining = safe_sub_u256(amount_in, x)?;
                let bal_in_after = safe_add_u256(bal_in, x)?;
                let bal_out_after = safe_sub_u256(bal_out, out)?;

                // Compare cross-products to decide which side of the ratio we are on.
                let left = safe_mul_u256(in_remaining, bal_out_after)?;
                let right = safe_mul_u256(out, bal_in_after)?;

                if left > right {
                    // Too much remaining input relative to swap output: increase swap amount.
                    lo = safe_add_u256(x, U256::from(1u8))?;
                } else {
                    // Swap is large enough; record candidate and try smaller to reduce dust.
                    best_x = x;
                    if x.is_zero() {
                        break;
                    }
                    hi = safe_sub_u256(x, U256::from(1u8))?;
                }
            }

            // Final swap leg using the best candidate x.
            let x = best_x;
            let out = calculate_out_given_in(bal_in, weight_in, bal_out, weight_out, x, fee)
                .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

            // Post-swap balances that will be used for the join leg.
            let in_remaining = safe_sub_u256(amount_in, x)?;
            let bal_in_after = safe_add_u256(bal_in, x)?;
            let bal_out_after = safe_sub_u256(bal_out, out)?;

            if bal_in_after.is_zero() || bal_out_after.is_zero() {
                return Err(SimulationError::FatalError(
                    "join_pool balance is zero after swap".to_string(),
                ));
            }

            // Compute the LP shares implied by each side, then take the minimum
            // to avoid over-minting relative to either asset.
            let pool_total = new_state.lp_token_supply;
            let lp_from_in = safe_div_u256(safe_mul_u256(in_remaining, pool_total)?, bal_in_after)?;
            let lp_from_out = safe_div_u256(safe_mul_u256(out, pool_total)?, bal_out_after)?;
            let mut lp_out = if lp_from_in < lp_from_out { lp_from_in } else { lp_from_out };

            if lp_out.is_zero() {
                return Err(SimulationError::FatalError(
                    "join_pool produces zero lp_out".to_string(),
                ));
            }
            // Update the local state with the post-swap balances before joining.
            if is_token_a_in {
                new_state.token_a.1 = bal_in_after;
                new_state.token_b.1 = bal_out_after;
            } else {
                new_state.token_b.1 = bal_in_after;
                new_state.token_a.1 = bal_out_after;
            }

            // These are the maximum amounts we are allowed to contribute to the join.
            let (max_a, max_b) =
                if is_token_a_in { (in_remaining, out) } else { (out, in_remaining) };

            // join_pool uses BONE-based rounding (bdiv/bmul), which can require slightly
            // more input than the floor-based amounts above. Reduce lp_out until the
            // implied required amounts fit within max_a/max_b.
            loop {
                let ratio = bdiv(lp_out, pool_total).map_err(|err| {
                    SimulationError::FatalError(format!("join_pool ratio error: {err:?}"))
                })?;
                let required_a = bmul(ratio, new_state.liquidity_a()).map_err(|err| {
                    SimulationError::FatalError(format!("join_pool amount_a error: {err:?}"))
                })?;
                let required_b = bmul(ratio, new_state.liquidity_b()).map_err(|err| {
                    SimulationError::FatalError(format!("join_pool amount_b error: {err:?}"))
                })?;

                if required_a <= max_a && required_b <= max_b {
                    break;
                }
                if lp_out.is_zero() {
                    return Err(SimulationError::FatalError(
                        "join_pool lp_out underflow while applying rounding tolerance".to_string(),
                    ));
                }
                lp_out = safe_sub_u256(lp_out, U256::from(1u8))?;
            }

            // Perform the join with the adjusted lp_out so the pool accepts the inputs.
            self.join_pool(&mut new_state, lp_out, &[max_a, max_b])
                .map_err(|err| SimulationError::FatalError(format!("join_pool error: {err:?}")))?;

            return Ok(GetAmountOutResult {
                amount: u256_to_biguint(lp_out),
                gas: 120_000u64.to_biguint().unwrap(),
                new_state: Box::new(new_state),
            });
        }

        // ============================
        // NORMAL SWAP
        // ============================
        let is_token_a_in = token_in.address == *self.token_a_addr();
        let (bal_in, weight_in, bal_out, weight_out) = if is_token_a_in {
            (self.liquidity_a(), self.weight_a(), self.liquidity_b(), self.weight_b())
        } else {
            (self.liquidity_b(), self.weight_b(), self.liquidity_a(), self.weight_a())
        };

        let amount_out = calculate_out_given_in(
            bal_in,
            weight_in,
            bal_out,
            weight_out,
            amount_in,
            U256::from(self.fee),
        )
        .map_err(|e| SimulationError::FatalError(format!("amount_out error: {e:?}")))?;

        if is_token_a_in {
            new_state.token_a.1 = safe_sub_u256(new_state.liquidity_a(), amount_in)?;
            new_state.token_b.1 = safe_add_u256(new_state.liquidity_b(), amount_out)?;
        } else {
            new_state.token_b.1 = safe_sub_u256(new_state.liquidity_b(), amount_in)?;
            new_state.token_a.1 = safe_add_u256(new_state.liquidity_a(), amount_out)?;
        }

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(amount_out),
            gas: 120_000u64.to_biguint().unwrap(),
            new_state: Box::new(new_state),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if self.liquidity_a().is_zero() || self.liquidity_b().is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }

        // Check if this is an LP token swap
        let is_lp_sell = sell_token == self.address;
        let is_lp_buy = buy_token == self.address;

        // Use special LP swap limits that account for intermediate swaps
        if is_lp_sell || is_lp_buy {
            return self.get_lp_swap_limits(is_lp_buy, sell_token, buy_token);
        }

        if sell_token == *self.token_a_addr() {
            // Sell token A for token B
            let max_in = bmul(self.liquidity_a(), MAX_IN_RATIO)
                .map_err(|err| SimulationError::FatalError(format!("max_in error: {err:?}")))?;

            let max_out = calculate_out_given_in(
                self.liquidity_a(),
                self.weight_a(),
                self.liquidity_b(),
                self.weight_b(),
                max_in,
                U256::from(self.fee),
            )
            .map_err(|err| SimulationError::FatalError(format!("max_out error: {err:?}")))?;

            Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
        } else {
            // Sell token B for token A
            let max_in = bmul(self.liquidity_b(), MAX_IN_RATIO)
                .map_err(|err| SimulationError::FatalError(format!("max_in error: {err:?}")))?;

            let max_out = calculate_out_given_in(
                self.liquidity_b(),
                self.weight_b(),
                self.liquidity_a(),
                self.weight_a(),
                max_in,
                U256::from(self.fee),
            )
            .map_err(|err| SimulationError::FatalError(format!("max_out error: {err:?}")))?;

            Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // liquidity_a, liquidity_b and lp_token_supply are considered required attributes and are
        // expected in every delta we process
        let liquidity_a = U256::from_be_slice(
            delta
                .updated_attributes
                .get("liquidity_a")
                .ok_or(TransitionError::MissingAttribute("liquidity_a".to_string()))?,
        );

        let liquidity_b = U256::from_be_slice(
            delta
                .updated_attributes
                .get("liquidity_b")
                .ok_or(TransitionError::MissingAttribute("liquidity_b".to_string()))?,
        );

        let lp_token_supply = U256::from_be_slice(
            delta
                .updated_attributes
                .get("lp_token_supply")
                .ok_or(TransitionError::MissingAttribute("lp_token_supply".to_string()))?,
        );

        self.token_a.1 = liquidity_a;
        self.token_b.1 = liquidity_b;
        self.lp_token_supply = lp_token_supply;

        Ok(())
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn transitions_from_delta_alone(&self, _delta: &ProtocolStateDelta) -> bool {
        true
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
            .downcast_ref::<CowAMMState>()
            .is_some_and(|other_state| self == other_state)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use alloy::primitives::U256;
    use approx::assert_ulps_eq;
    use num_bigint::BigUint;
    use num_traits::{One, ToPrimitive, Zero};
    use rstest::rstest;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{token::Token, Chain},
        simulation::errors::{SimulationError, TransitionError},
        Bytes,
    };

    use super::*;
    use crate::evm::protocol::{
        cowamm::state::{CowAMMState, ProtocolSim},
        u256_num::biguint_to_u256,
    };
    /// Converts a `BigUint` amount in wei to f64 ETH/WETH
    pub fn wei_to_eth(amount: &BigUint) -> f64 {
        // 1 ETH = 10^18 wei
        let divisor = 1e18_f64;
        amount.to_f64().unwrap_or(0.0) / divisor
    }

    fn create_test_tokens() -> (Token, Token, Token, Token, Token, Token, Token) {
        let t0 = Token::new(
            &Bytes::from_str("0xDEf1CA1fb7FBcDC777520aa7f396b4E015F497aB").unwrap(),
            "COW",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let t1 = Token::new(
            &Bytes::from_str("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0").unwrap(),
            "wstETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let t2 = Token::new(
            &Bytes::from_str("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1").unwrap(),
            "BCoW-50CoW-50wstETH",
            18,
            0,
            &[Some(199_999_999_999_999_990)],
            Chain::Ethereum,
            100,
        );
        let t3 = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(1_000_000)],
            Chain::Ethereum,
            100,
        );
        let t4 = Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000_000)],
            Chain::Ethereum,
            100,
        );
        let t5 = Token::new(
            &Bytes::from_str("0xBAac2B4491727D78D2b78815144570b9f2Fe8899").unwrap(),
            "DOG",
            18,
            0,
            &[Some(10_000_000)],
            Chain::Ethereum,
            100,
        );
        let t6 = Token::new(
            &Bytes::from_str("0x9d0e8cdf137976e03ef92ede4c30648d05e25285").unwrap(),
            "wstETH-DOG-LP-Token",
            18,
            0,
            &[Some(10_000_000)],
            Chain::Ethereum,
            100,
        );
        (t0, t1, t2, t3, t4, t5, t6)
    }

    #[rstest]
    #[case::same_dec(
        U256::from_str("1547000000000000000000").unwrap(), //COW balance
        U256::from_str("100000000000000000").unwrap(), //wstETH balance
        0, 1, // token indices: t0 -> t1
        BigUint::from_str("5205849666").unwrap(),  //COW in
        BigUint::from_str("336513").unwrap(), //wstETH out
    )]
    #[case::test_dec(
        U256::from_str("81297577909021519893").unwrap(), //wstETH balance
        U256::from_str("332162411254631243300976822").unwrap(), //DOG balance
        1, 5, // token indices: t1 -> t5
        BigUint::from_str("1000000000000000000").unwrap(),  //wstETH in
        BigUint::from_str("4036114059417772362872299").unwrap(), //DOG out
    )]
    #[case::diff_dec(
        U256::from_str("170286779513658066185").unwrap(), //WETH balance
        U256::from_str("413545982676").unwrap(), //USDC balance
        3, 4, // token indices: t3 -> t4
        BigUint::from_str("217679081735374278").unwrap(), //WETH in
        BigUint::from_str("527964550").unwrap(), // USDC out (≈ 0.2177 WETH) for 527964550 USDC (≈ 527.96 USDC)
    )]
    fn test_get_amount_out(
        #[case] liq_a: U256,
        #[case] liq_b: U256,
        #[case] token_in_idx: usize,
        #[case] token_out_idx: usize,
        #[case] amount_in: BigUint,
        #[case] expected_out: BigUint,
    ) {
        let (t0, t1, t2, t3, t4, t5, t6) = create_test_tokens();
        let tokens = [&t0, &t1, &t2, &t3, &t4, &t5, &t6];
        let token_in = tokens[token_in_idx];
        let token_out = tokens[token_out_idx];

        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            token_in.address.clone(),
            token_out.address.clone(),
            liq_a, //wstETH
            liq_b, //DOG
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            U256::from_str("128375712183366405029").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let res = state
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .unwrap();

        assert_eq!(res.amount, expected_out);

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<CowAMMState>()
            .unwrap();

        assert_eq!(
            new_state.liquidity_a(),
            safe_sub_u256(liq_a, biguint_to_u256(&amount_in)).unwrap()
        );
        assert_eq!(
            new_state.liquidity_b(),
            safe_add_u256(liq_b, biguint_to_u256(&expected_out)).unwrap()
        );

        // Original state unchanged
        assert_eq!(state.liquidity_a(), liq_a);
        assert_eq!(state.liquidity_b(), liq_b);
    }

    #[rstest]
    #[case::buy_lp_token( //buying lp token with COW == buying amounts of wstETH needed to join pool with COW, then joining pool with both tokens
        U256::from_str("81297577909021519893").unwrap(),      // wstETH balance
        U256::from_str("332162411254631243300976822").unwrap(), //DOG balance
        1, 6, // token indices: wstETH -> wstETH-DOG-LP-Token
        BigUint::from_str("1000000000000000000").unwrap(), // wstETH in
        BigUint::from_str("787128927353433245").unwrap(), // Amount of wstETH-DOG-LP-Token we buy from join the pool
    )]
    fn test_get_amount_out_buy_lp_token(
        #[case] liq_a: U256,
        #[case] liq_b: U256,
        #[case] token_in_idx: usize,
        #[case] token_out_idx: usize,
        #[case] amount_in: BigUint,
        #[case] expected_out: BigUint,
    ) {
        let (t0, t1, t2, t3, t4, t5, t6) = create_test_tokens();
        let tokens = [&t0, &t1, &t2, &t3, &t4, &t5, &t6];

        let token_a = tokens[token_in_idx];
        let token_b = tokens[token_out_idx];

        let state = CowAMMState::new(
            Bytes::from("0x9d0e8cdf137976e03ef92ede4c30648d05e25285"),
            t1.address.clone(), //wstETH
            t5.address.clone(), //DOG
            liq_a,              //wstETH Liquidity
            liq_b,              //DOG Liquidity
            Bytes::from("0x9d0e8cdf137976e03ef92ede4c30648d05e25285"),
            U256::from_str("128375712183366405029").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let res = state
            .get_amount_out(amount_in.clone(), token_a, token_b)
            .unwrap();

        assert_eq!(res.amount, expected_out);
        //lp token supply reduced
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<CowAMMState>()
            .unwrap();

        assert!(
            new_state.lp_token_supply > state.lp_token_supply,
            "LP token supply did not increase"
        );

        // Original state unchanged
        assert_eq!(state.liquidity_a(), liq_a);
        assert_eq!(state.liquidity_b(), liq_b);
    }

    #[rstest]
    #[case::sell_lp_token( //selling (redeeming) lp_token for COW == exiting pool and converting excess COW to wstETH
        U256::from_str("1547000000000000000000").unwrap(),
        U256::from_str("100000000000000000").unwrap(),
        2, 0, // token indices: t2 -> t0
        BigUint::from_str("1000000000000000000").unwrap(), //Amount of lp_token being sold (redeeemd)
        BigUint::from_str("15431325000000000000").unwrap(), //COW as output, verify that we sent this amount to the pool in total
    )]
    fn test_get_amount_out_sell_lp_token(
        #[case] liq_a: U256,
        #[case] liq_b: U256,
        #[case] token_in_idx: usize,
        #[case] token_out_idx: usize,
        #[case] amount_in: BigUint,
        #[case] expected_out: BigUint,
    ) {
        let (t0, t1, t2, t3, t4, t5, t6) = create_test_tokens();
        let tokens = [&t0, &t1, &t2, &t3, &t4, &t5, &t6];

        let token_a = tokens[token_in_idx];
        let token_b = tokens[token_out_idx];

        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(), //COW
            t1.address.clone(), //wstETH
            liq_a,              //COW
            liq_b,              //wstETH
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            U256::from_str("199999999999999999990").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let res = state
            .get_amount_out(amount_in.clone(), token_a, token_b)
            .unwrap();

        assert_eq!(res.amount, expected_out);
        //lp token supply reduced
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<CowAMMState>()
            .unwrap();

        if token_a.address == t2.address {
            assert!(
                new_state.lp_token_supply < state.lp_token_supply,
                "LP token supply did not reduce"
            );
        } else {
            assert!(
                new_state.lp_token_supply > state.lp_token_supply,
                "LP token supply did not reduce"
            );
        }

        // Original state unchanged
        assert_eq!(state.liquidity_a(), liq_a);
        assert_eq!(state.liquidity_b(), liq_b);
    }

    #[test]
    fn test_get_amount_out_overflow() {
        let max = (BigUint::one() << 256) - BigUint::one();

        let (t0, t1, _, _, _, _, _) = create_test_tokens();

        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(),
            t1.address.clone(),
            U256::from_str("886800000000000000").unwrap(),
            U256::from_str("50000000000000000").unwrap(),
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            U256::from_str("100000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let res = state.get_amount_out(max, &t0.clone(), &t1.clone());
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert!(matches!(err, SimulationError::FatalError(_)));
    }

    #[rstest]
    #[case(244752492017f64)]
    fn test_spot_price(#[case] expected: f64) {
        let (t0, _, _, _, _, t5, _) = create_test_tokens();
        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(),
            t5.address.clone(),
            U256::from_str("81297577909021519893").unwrap(),
            U256::from_str("332162411254631243300976822").unwrap(),
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            U256::from_str("128375712183366405029").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let price = state.spot_price(&t0, &t5).unwrap();
        assert_ulps_eq!(price, expected);
    }

    #[test]
    fn test_fee() {
        let (t0, t1, _, _, _, _, _) = create_test_tokens();

        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(),
            t1.address.clone(),
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            0,
        );

        let res = state.fee();

        assert_ulps_eq!(res, 0.0);
    }
    #[test]
    fn test_delta_transition() {
        let (t0, t1, _, _, _, _, _) = create_test_tokens();

        let mut state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(),
            t1.address.clone(),
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            0,
        );
        let attributes: HashMap<String, Bytes> = vec![
            ("liquidity_a".to_string(), Bytes::from(15000_u64.to_be_bytes().to_vec())),
            ("liquidity_b".to_string(), Bytes::from(20000_u64.to_be_bytes().to_vec())),
            ("lp_token_supply".to_string(), Bytes::from(250000_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_ok());
        assert_eq!(state.liquidity_a(), U256::from_str("15000").unwrap());
        assert_eq!(state.liquidity_b(), U256::from_str("20000").unwrap());
        assert_eq!(state.lp_token_supply, U256::from_str("250000").unwrap());
    }

    #[test]
    fn test_delta_transition_missing_attribute() {
        let mut state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            Bytes::from("0xDEf1CA1fb7FBcDC777520aa7f396b4E015F497aB"),
            Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
            U256::from_str("36928554990972").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
            0,
        );
        let attributes: HashMap<String, Bytes> = vec![(
            "liquidity_a".to_string(),
            Bytes::from(
                1500000000000000_u64
                    .to_be_bytes()
                    .to_vec(),
            ),
        )]
        .into_iter()
        .collect();

        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_err());
        match res {
            Err(e) => {
                assert!(matches!(e, TransitionError::MissingAttribute(ref x) if x== "liquidity_b"))
            }
            _ => panic!("Test failed: was expecting an Err value"),
        };
    }

    #[test]
    fn test_get_limits_price_impact() {
        let (t0, t1, _, _, _, _, _) = create_test_tokens();

        let state = CowAMMState::new(
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            t0.address.clone(),
            t1.address.clone(),
            U256::from_str("886800000000000000").unwrap(),
            U256::from_str("50000000000000000").unwrap(),
            Bytes::from("0x9bd702E05B9c97E4A4a3E47Df1e0fe7A0C26d2F1"),
            U256::from_str("100000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        let (amount_in, _) = state
            .get_limits(
                Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            )
            .unwrap();

        let t0 = Token::new(
            &Bytes::from_str("0xDEf1CA1fb7FBcDC777520aa7f396b4E015F497aB").unwrap(),
            "COW",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let t1 = Token::new(
            &Bytes::from_str("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0").unwrap(),
            "wstETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let result = state
            .get_amount_out(amount_in.clone(), &t0, &t1)
            .unwrap();
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<CowAMMState>()
            .unwrap();

        let initial_price = state.spot_price(&t0, &t1).unwrap();
        println!("Initial spot price (t0 -> t1): {}", initial_price);

        let new_price = new_state.spot_price(&t0, &t1).unwrap();

        println!("New spot price (t0 -> t1), floored: {}", new_price);

        assert!(new_price < initial_price);
    }
    #[test]
    fn test_arb_weth_lp_limits_calculation() {
        let arb = Token::new(
            &Bytes::from_str("0xb50721bcf8d664c30412cfbc6cf7a15145234ad1").unwrap(),
            "ARB",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let lp_token = Token::new(
            &Bytes::from_str("0x4359a8ea4c353d93245c0b6b8608a28bb48a05e2").unwrap(),
            "BCoW-50ARB-50WETH",
            18,
            0,
            &[Some(60_502_268_657_704_388)],
            Chain::Ethereum,
            100,
        );

        let state = CowAMMState::new(
            Bytes::from_str("0x4359a8ea4c353d93245c0b6b8608a28bb48a05e2").unwrap(),
            arb.address.clone(),
            weth.address.clone(),
            U256::from_str("286275852074040134274570").unwrap(), //ARB
            U256::from_str("61694306956323018369").unwrap(),     //WETH
            Bytes::from_str("0x4359a8ea4c353d93245c0b6b8608a28bb48a05e2").unwrap(),
            U256::from_str("60502268657704388057834").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            U256::from_str("1000000000000000000").unwrap(),
            0,
        );

        //Get limits for LP → WETH
        //uses lp_limits
        let (max_lp_in, max_weth_out) = state
            .get_limits(lp_token.address.clone(), weth.address.clone())
            .unwrap();

        println!("LP → WETH limits:");
        println!("  Max LP in: {}", max_lp_in);
        println!("  Max WETH out: {:.6} WETH", wei_to_eth(&max_weth_out));

        // Test with 10% of the SAFE max
        let amount_in = max_lp_in.clone() / BigUint::from(10u64);
        println!("\nTesting with 10% of safe max: {}", amount_in);

        let res = state
            .get_amount_out(amount_in.clone(), &lp_token, &weth)
            .expect("Should succeed with safe limit");
        // Basic sanity assertions
        assert!(!res.amount.is_zero(), "Amount out should be non-zero for a valid LP redemption");
    }
}
