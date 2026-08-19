use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, Price, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::evm::{
    protocol::{
        safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    },
    query_pool_swap::is_within_tolerance,
};

const WAD: u128 = 1_000_000_000_000_000_000;
/// Sentinel for `tin`/`tout` disabling the respective swap direction (DssLitePsm.HALTED).
const HALTED: U256 = U256::MAX;

const PSM_SWAP_GAS: u64 = 120_000;
const WRAPPER_SWAP_GAS: u64 = 220_000;
const CONVERTER_SWAP_GAS: u64 = 100_000;

/// Behaviour of a `sky` protocol component, mapped from its `component_type`
/// static attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkyComponentKind {
    /// DssLitePsm: stable (18-dec DAI) <-> gem (USDC) at 1:1 with `tin`/`tout` fees.
    Psm,
    /// UsdsPsmWrapper: same PSM math with USDS as the stable side.
    PsmWrapper,
    /// DaiUsds: 1:1 mint/burn between two 18-dec stables, feeless and immutable.
    Converter,
}

/// The join escrows (`vat.dai[join]`, wad) bounding the wrapper's in-flight
/// DAI <-> USDS conversion legs: `daiToUsds`/`usdsToDai` burn through `join`, which
/// debits the respective join's escrow and reverts beyond it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinEscrows {
    pub dai: U256,
    pub usds: U256,
}

/// State for Sky mint/redeem components (LitePSM, USDS PSM wrapper, DaiUsds converter).
///
/// All components swap 1:1 modulo decimal rescaling; the PSM legs additionally apply the
/// governance-mutable `tin` (stable out) / `tout` (stable in) wad fees. Balances bound
/// the swap limits: the PSM's pre-minted stable inventory and pocket gem inventory, or
/// the join escrows (`vat.dai[join]`, the burnable amount per side) for the converter.
/// The wrapper is additionally bounded by both join escrows, which its in-flight
/// DAI <-> USDS conversion burns through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkyState {
    pub component_id: String,
    pub kind: SkyComponentKind,
    /// The side paired against the gem: DAI (psm, converter) or USDS (wrapper).
    stable: Token,
    /// The side named by the `gem` static attribute, which fee and call direction
    /// are defined against: USDC (psm, wrapper) or USDS (converter).
    gem: Token,
    /// Fee in wad taken on gem -> stable swaps (`sellGem`); `U256::MAX` halts them.
    tin: U256,
    /// Fee in wad taken on stable -> gem swaps (`buyGem`); `U256::MAX` halts them.
    tout: U256,
    stable_balance: U256,
    gem_balance: U256,
    /// The join escrows the component's conversion legs burn through; wrapper only.
    escrows: Option<JoinEscrows>,
}

impl SkyState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component_id: String,
        kind: SkyComponentKind,
        stable: Token,
        gem: Token,
        tin: U256,
        tout: U256,
        stable_balance: U256,
        gem_balance: U256,
        escrows: Option<JoinEscrows>,
    ) -> Self {
        Self { component_id, kind, stable, gem, tin, tout, stable_balance, gem_balance, escrows }
    }

    /// 10^(stable.decimals - gem.decimals); 1 for the converter's 18/18 pair.
    fn conversion_factor(&self) -> U256 {
        U256::from(10).pow(U256::from(self.stable.decimals - self.gem.decimals))
    }

    fn swap_gas(&self) -> u64 {
        match self.kind {
            SkyComponentKind::Psm => PSM_SWAP_GAS,
            SkyComponentKind::PsmWrapper => WRAPPER_SWAP_GAS,
            SkyComponentKind::Converter => CONVERTER_SWAP_GAS,
        }
    }

    fn is_gem_to_stable(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<bool, SimulationError> {
        if *token_in == self.gem.address && *token_out == self.stable.address {
            Ok(true)
        } else if *token_in == self.stable.address && *token_out == self.gem.address {
            Ok(false)
        } else {
            Err(SimulationError::InvalidInput(
                format!("invalid token pair: {token_in}, {token_out}"),
                None,
            ))
        }
    }

    /// `sellGem` output: stable amount received for `gem_in`, after `tin`.
    fn stable_out(&self, gem_in: U256) -> Result<U256, SimulationError> {
        if self.tin == HALTED {
            return Err(SimulationError::RecoverableError("sell gem is halted".to_string()));
        }
        let stable_wad = safe_mul_u256(gem_in, self.conversion_factor())?;
        let fee = safe_mul_u256(stable_wad, self.tin)? / U256::from(WAD);
        safe_sub_u256(stable_wad, fee)
    }

    /// `buyGem` output: the largest gem amount whose cost (incl. `tout`) fits in
    /// `stable_in`. Rounds down, so execution never requires more than `stable_in`.
    fn gem_out(&self, stable_in: U256) -> Result<U256, SimulationError> {
        if self.tout == HALTED {
            return Err(SimulationError::RecoverableError("buy gem is halted".to_string()));
        }
        let wad = U256::from(WAD);
        // The divisor cannot be zero: a power of ten times at least `wad`.
        Ok(safe_mul_u256(stable_in, wad)? /
            safe_mul_u256(self.conversion_factor(), safe_add_u256(wad, self.tout)?)?)
    }

    fn apply_component_balance_updates(&mut self, balances: &Balances) {
        let Some(component_balances) = balances
            .component_balances
            .get(&self.component_id)
        else {
            return;
        };
        if let Some(balance) = component_balances.get(&self.stable.address) {
            self.stable_balance = U256::from_be_slice(balance);
        }
        if let Some(balance) = component_balances.get(&self.gem.address) {
            self.gem_balance = U256::from_be_slice(balance);
        }
    }

    fn fee_f64(fee: U256) -> f64 {
        if fee == HALTED {
            return 1.0;
        }
        u256_to_f64(fee).unwrap_or(f64::MAX) / WAD as f64
    }
}

#[typetag::serde]
impl ProtocolSim for SkyState {
    fn fee(&self) -> f64 {
        f64::max(Self::fee_f64(self.tin), Self::fee_f64(self.tout))
    }

    /// Buy price of `base` in `quote` (trait convention): acquiring gem goes through
    /// `buyGem` and costs `1 + tout` stable per unit; acquiring stable goes through
    /// `sellGem`, where receiving 1 stable costs `1 / (1 - tin)` gem.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if self.is_gem_to_stable(&base.address, &quote.address)? {
            if self.tout == HALTED {
                return Err(SimulationError::RecoverableError("buy gem is halted".to_string()));
            }
            Ok(1.0 + Self::fee_f64(self.tout))
        } else {
            if self.tin == HALTED {
                return Err(SimulationError::RecoverableError("sell gem is halted".to_string()));
            }
            Ok(1.0 / (1.0 - Self::fee_f64(self.tin)))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        let mut new_state = self.clone();
        let gem_to_stable = self.is_gem_to_stable(&token_in.address, &token_out.address)?;

        // The converter mints its output and burns its input, so its balances (the
        // join escrows) move opposite to the PSM's inventories: burning shrinks the
        // input side's escrow, minting grows the output side's. The output never
        // bounds a swap — only the input (a burn cannot exceed the escrow backing
        // the circulating supply).
        let amount_out = if self.kind == SkyComponentKind::Converter {
            let (sell_escrow, out) = if gem_to_stable {
                (self.gem_balance, self.stable_out(amount_in)?)
            } else {
                (self.stable_balance, self.gem_out(amount_in)?)
            };
            if amount_in > sell_escrow {
                return Err(SimulationError::RecoverableError(format!(
                    "amount in {amount_in} exceeds sell token escrow {sell_escrow}"
                )));
            }
            if gem_to_stable {
                new_state.gem_balance -= amount_in;
                new_state.stable_balance += out;
            } else {
                new_state.stable_balance -= amount_in;
                new_state.gem_balance += out;
            }
            out
        } else if gem_to_stable {
            let out = self.stable_out(amount_in)?;
            if out > self.stable_balance {
                return Err(SimulationError::RecoverableError(format!(
                    "amount out {out} exceeds stable inventory {}",
                    self.stable_balance
                )));
            }
            // The wrapper converts the PSM's DAI payout to USDS in-flight, burning it
            // through DaiJoin: the escrow bounds the payout, and the burnt DAI backs
            // the minted USDS on the other join.
            if let Some(escrows) = &mut new_state.escrows {
                if out > escrows.dai {
                    return Err(SimulationError::RecoverableError(format!(
                        "amount out {out} exceeds DAI join escrow {}",
                        escrows.dai
                    )));
                }
                escrows.dai -= out;
                escrows.usds += out;
            }
            new_state.stable_balance -= out;
            new_state.gem_balance += amount_in;
            out
        } else {
            let out = self.gem_out(amount_in)?;
            if out > self.gem_balance {
                return Err(SimulationError::RecoverableError(format!(
                    "amount out {out} exceeds gem inventory {}",
                    self.gem_balance
                )));
            }
            // The wrapper burns the full USDS input through UsdsJoin before buying gem
            // from the PSM; the escrow bounds the input.
            if let Some(escrows) = &mut new_state.escrows {
                if amount_in > escrows.usds {
                    return Err(SimulationError::RecoverableError(format!(
                        "amount in {amount_in} exceeds USDS join escrow {}",
                        escrows.usds
                    )));
                }
                escrows.usds -= amount_in;
                escrows.dai += amount_in;
            }
            new_state.gem_balance -= out;
            new_state.stable_balance += amount_in;
            out
        };

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(amount_out),
            gas: BigUint::from(self.swap_gas()),
            new_state: Box::new(new_state),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let wad = U256::from(WAD);
        if self.is_gem_to_stable(&sell_token, &buy_token)? {
            // Bounded by the pre-minted stable inventory (for the converter: by the
            // sell token's join escrow, tracked as the gem balance).
            if self.tin == HALTED {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }
            if self.kind == SkyComponentKind::Converter {
                return Ok((u256_to_biguint(self.gem_balance), u256_to_biguint(self.gem_balance)));
            }
            // The wrapper's stable payout is additionally bounded by the DAI join
            // escrow its in-flight conversion burns through.
            let max_out = match &self.escrows {
                Some(escrows) => self.stable_balance.min(escrows.dai),
                None => self.stable_balance,
            };
            let max_in = safe_div_u256(
                safe_mul_u256(max_out, wad)?,
                safe_mul_u256(self.conversion_factor(), safe_sub_u256(wad, self.tin)?)?,
            )?;
            Ok((u256_to_biguint(max_in), u256_to_biguint(self.stable_out(max_in)?)))
        } else {
            if self.tout == HALTED {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }
            if self.kind == SkyComponentKind::Converter {
                return Ok((
                    u256_to_biguint(self.stable_balance),
                    u256_to_biguint(self.stable_balance),
                ));
            }
            // Bounded by the pocket's gem inventory; for the wrapper additionally by
            // the USDS join escrow the full stable input is burned through.
            let max_out = self.gem_balance;
            let max_in = safe_mul_u256(
                safe_mul_u256(max_out, self.conversion_factor())?,
                safe_add_u256(wad, self.tout)?,
            )? / wad;
            if let Some(escrows) = &self.escrows {
                if escrows.usds < max_in {
                    return Ok((
                        u256_to_biguint(escrows.usds),
                        u256_to_biguint(self.gem_out(escrows.usds)?),
                    ));
                }
            }
            Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(tin) = delta.updated_attributes.get("tin") {
            self.tin = U256::from_be_slice(tin);
        }
        if let Some(tout) = delta.updated_attributes.get("tout") {
            self.tout = U256::from_be_slice(tout);
        }
        if let Some(escrows) = &mut self.escrows {
            if let Some(dai) = delta
                .updated_attributes
                .get("dai_escrow")
            {
                escrows.dai = U256::from_be_slice(dai);
            }
            if let Some(usds) = delta
                .updated_attributes
                .get("usds_escrow")
            {
                escrows.usds = U256::from_be_slice(usds);
            }
        }
        self.apply_component_balance_updates(balances);
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
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<SkyState>()
        {
            self == other_state
        } else {
            false
        }
    }

    /// Closed-form implementation: prices are size-independent up to the hard
    /// capacity bound, so the crate's generic root-search (built for falling AMM
    /// price curves, where it brackets and converges on an interior amount) would
    /// return an arbitrary probe instead of the true optimum on a flat curve.
    ///
    /// - `TradeLimitPrice` is all-or-nothing: the full `get_limits` capacity if the flat execution
    ///   price clears the limit, a zero-amount swap otherwise.
    /// - `PoolTargetPrice` is only satisfiable where the pool already is: a zero-amount swap for a
    ///   target within tolerance of spot, an error otherwise.
    ///
    /// `min_amount_in`/`max_amount_in` are ignored, matching the generic helper.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let token_in = params.token_in();
        let token_out = params.token_out();
        let zero_swap =
            || PoolSwap::new(BigUint::from(0u8), BigUint::from(0u8), self.clone_box(), None);

        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { limit, .. } => {
                // Zero limits cover both empty inventory and a HALTED direction.
                let (max_in, _) =
                    self.get_limits(token_in.address.clone(), token_out.address.clone())?;
                if max_in == BigUint::from(0u8) {
                    return Ok(zero_swap());
                }
                // The flat execution price of the trade (out per in, decimal
                // adjusted). Not `spot_price`: that is the buy price of `token_in`,
                // i.e. the opposite trade with the opposite fee.
                let execution = if self.is_gem_to_stable(&token_in.address, &token_out.address)? {
                    1.0 - Self::fee_f64(self.tin)
                } else {
                    1.0 / (1.0 + Self::fee_f64(self.tout))
                };
                let limit = price_f64(limit, token_in.decimals, token_out.decimals);
                if execution < limit {
                    return Ok(zero_swap());
                }
                // `get_amount_out` is only needed for the post-swap state; its
                // amount matches the `get_limits` max_out by construction.
                let result = self.get_amount_out(max_in.clone(), token_in, token_out)?;
                Ok(PoolSwap::new(max_in, result.amount, result.new_state, None))
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
                let target = price_f64(target, token_in.decimals, token_out.decimals);
                let spot = self.spot_price(token_in, token_out)?;
                if is_within_tolerance(spot, target, *tolerance) {
                    return Ok(zero_swap());
                }
                Err(SimulationError::InvalidInput(
                    format!("spot price {spot} is size-independent; cannot reach target {target}"),
                    None,
                ))
            }
        }
    }
}

/// Decimal-adjusted f64 of a `Price` (`token_out` per `token_in`).
fn price_f64(price: &Price, in_decimals: u32, out_decimals: u32) -> f64 {
    (to_f64(&price.numerator, out_decimals)) / (to_f64(&price.denominator, in_decimals))
}

fn to_f64(amount: &BigUint, decimals: u32) -> f64 {
    amount.to_f64().unwrap_or(f64::MAX) / 10f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;

    fn dai() -> Token {
        Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "DAI",
            18,
            0,
            &[Some(50_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(50_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usds() -> Token {
        Token::new(
            &Bytes::from_str("0xdc035d45d973e3ec169d2276ddab16f1e407384f").unwrap(),
            "USDS",
            18,
            0,
            &[Some(50_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn wad(amount: u64) -> U256 {
        U256::from(amount) * U256::from(WAD)
    }

    fn psm_state(tin: U256, tout: U256) -> SkyState {
        SkyState::new(
            "0xf6e72db5454dd049d0788e411b06cfaf16853042".to_string(),
            SkyComponentKind::Psm,
            dai(),
            usdc(),
            tin,
            tout,
            wad(1_000_000),                   // 1M DAI buffer
            U256::from(2_000_000_000_000u64), // 2M USDC in the pocket
            None,
        )
    }

    fn wrapper_state(escrows: JoinEscrows) -> SkyState {
        SkyState::new(
            "0xa188eec8f81263234da3622a406892f3d630f98c".to_string(),
            SkyComponentKind::PsmWrapper,
            usds(),
            usdc(),
            U256::ZERO,
            U256::ZERO,
            wad(1_000_000),                   // mirrored 1M DAI buffer
            U256::from(2_000_000_000_000u64), // mirrored 2M USDC pocket
            Some(escrows),
        )
    }

    fn converter_state() -> SkyState {
        SkyState::new(
            "0x3225737a9bbb6473cb4a45b7244aca2befdb276a".to_string(),
            SkyComponentKind::Converter,
            dai(),
            usds(),
            U256::ZERO,
            U256::ZERO,
            wad(3_000_000), // DaiJoin escrow
            wad(9_000_000), // UsdsJoin escrow
            None,
        )
    }

    #[test]
    fn sell_gem_fee_free_rescales_decimals() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        let res = state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &dai())
            .unwrap();
        // 1 USDC -> 1 DAI
        assert_eq!(res.amount, BigUint::from(WAD));
    }

    #[test]
    fn buy_gem_fee_free_rescales_decimals() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        let res = state
            .get_amount_out(BigUint::from(WAD), &dai(), &usdc())
            .unwrap();
        // 1 DAI -> 1 USDC
        assert_eq!(res.amount, BigUint::from(1_000_000u64));
    }

    #[test]
    fn sell_gem_applies_tin() {
        // tin = 0.1% = 1e15
        let state = psm_state(U256::from(WAD / 1000), U256::ZERO);
        let res = state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &dai())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(WAD - WAD / 1000));
    }

    #[test]
    fn buy_gem_applies_tout_and_rounds_down() {
        // tout = 0.1%: 1 DAI buys floor(1e18 * 1e18 / (1e12 * 1.001e18)) = 999000 (dust) USDC
        let state = psm_state(U256::ZERO, U256::from(WAD / 1000));
        let res = state
            .get_amount_out(BigUint::from(WAD), &dai(), &usdc())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(999_000u64));
        // The implied cost of the returned amount never exceeds the input.
        let cost = U256::from(999_000u64) * U256::from(1_000_000_000_000u64) * // ->wad
            (U256::from(WAD) + U256::from(WAD / 1000)) /
            U256::from(WAD);
        assert!(cost <= wad(1));
    }

    #[test]
    fn swap_updates_inventory_in_new_state() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        let res = state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &dai())
            .unwrap();
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<SkyState>()
            .unwrap()
            .clone();
        assert_eq!(new_state.stable_balance, state.stable_balance - U256::from(WAD));
        assert_eq!(new_state.gem_balance, state.gem_balance + U256::from(1_000_000u64));
    }

    #[test]
    fn sell_gem_bounded_by_stable_inventory() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        // 2M USDC in would need 2M DAI out but only 1M is pre-minted.
        let res = state.get_amount_out(BigUint::from(2_000_000_000_000u64), &usdc(), &dai());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[rstest]
    #[case::sell_gem_halted(HALTED, U256::ZERO, true)]
    #[case::buy_gem_halted(U256::ZERO, HALTED, false)]
    fn halted_direction_has_zero_limits(
        #[case] tin: U256,
        #[case] tout: U256,
        #[case] gem_to_stable: bool,
    ) {
        let state = psm_state(tin, tout);
        let (sell, buy) = if gem_to_stable {
            state
                .get_limits(usdc().address, dai().address)
                .unwrap()
        } else {
            state
                .get_limits(dai().address, usdc().address)
                .unwrap()
        };
        assert_eq!(sell, BigUint::ZERO);
        assert_eq!(buy, BigUint::ZERO);
    }

    #[test]
    fn limits_match_inventory() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        let (max_in, max_out) = state
            .get_limits(usdc().address, dai().address)
            .unwrap();
        // Sell side capped by the 1M DAI buffer.
        assert_eq!(max_out, u256_to_biguint(wad(1_000_000)));
        assert_eq!(max_in, BigUint::from(1_000_000_000_000u64));

        let (max_in, max_out) = state
            .get_limits(dai().address, usdc().address)
            .unwrap();
        // Buy side capped by the 2M USDC pocket.
        assert_eq!(max_out, BigUint::from(2_000_000_000_000u64));
        assert_eq!(max_in, u256_to_biguint(wad(2_000_000)));
    }

    #[test]
    fn spot_prices_include_fees_and_round_trip() {
        let tin = U256::from(WAD / 1000);
        let tout = U256::from(2 * (WAD / 1000));
        let state = psm_state(tin, tout);
        let usdc_in_dai = state
            .spot_price(&usdc(), &dai())
            .unwrap();
        let dai_in_usdc = state
            .spot_price(&dai(), &usdc())
            .unwrap();
        // Buy prices: acquiring USDC costs 1 + tout DAI; acquiring DAI costs
        // 1 / (1 - tin) USDC. The round trip is >= 1 under the buy-price convention.
        assert!((usdc_in_dai - 1.002).abs() < 1e-12);
        assert!((dai_in_usdc - 1.0 / 0.999).abs() < 1e-12);
        assert!(usdc_in_dai * dai_in_usdc >= 1.0);
    }

    #[test]
    fn converter_is_symmetric_one_to_one() {
        let state = converter_state();
        let res = state
            .get_amount_out(BigUint::from(WAD), &usds(), &dai())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(WAD));
        let res = state
            .get_amount_out(BigUint::from(WAD), &dai(), &usds())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(WAD));
        assert_eq!(
            state
                .spot_price(&dai(), &usds())
                .unwrap(),
            1.0
        );
    }

    #[test]
    fn converter_mints_output_regardless_of_target_escrow() {
        // USDS's escrow was zero at converter creation: DAI -> USDS must still work,
        // and the escrows move burn-in / mint-out.
        let state = SkyState::new(
            "0x3225737a9bbb6473cb4a45b7244aca2befdb276a".to_string(),
            SkyComponentKind::Converter,
            dai(),
            usds(),
            U256::ZERO,
            U256::ZERO,
            wad(3_000_000),
            U256::ZERO,
            None,
        );
        let res = state
            .get_amount_out(BigUint::from(WAD), &dai(), &usds())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(WAD));
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<SkyState>()
            .unwrap();
        assert_eq!(new_state.stable_balance, wad(3_000_000) - U256::from(WAD));
        assert_eq!(new_state.gem_balance, U256::from(WAD));
    }

    #[test]
    fn converter_burn_bounded_by_sell_escrow() {
        let state = converter_state();
        // More USDS than the escrow backs cannot be burned.
        let res = state.get_amount_out(u256_to_biguint(wad(9_000_001)), &usds(), &dai());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn converter_limits_use_sell_token_escrow() {
        let state = converter_state();
        let (max_in, _) = state
            .get_limits(dai().address, usds().address)
            .unwrap();
        assert_eq!(max_in, u256_to_biguint(wad(3_000_000)));
        let (max_in, _) = state
            .get_limits(usds().address, dai().address)
            .unwrap();
        assert_eq!(max_in, u256_to_biguint(wad(9_000_000)));
    }

    #[test]
    fn wrapper_stable_payout_bounded_by_dai_escrow() {
        // DAI escrow smaller than the mirrored 1M buffer binds the USDC -> USDS side.
        let state = wrapper_state(JoinEscrows { dai: wad(400_000), usds: wad(9_000_000) });
        let (max_in, max_out) = state
            .get_limits(usdc().address, usds().address)
            .unwrap();
        assert_eq!(max_out, u256_to_biguint(wad(400_000)));
        assert_eq!(max_in, BigUint::from(400_000_000_000u64));

        let res = state.get_amount_out(BigUint::from(500_000_000_000u64), &usdc(), &usds());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn wrapper_stable_input_bounded_by_usds_escrow() {
        // USDS escrow smaller than the pocket-implied input binds the USDS -> USDC side.
        let state = wrapper_state(JoinEscrows { dai: wad(3_000_000), usds: wad(500_000) });
        let (max_in, max_out) = state
            .get_limits(usds().address, usdc().address)
            .unwrap();
        assert_eq!(max_in, u256_to_biguint(wad(500_000)));
        assert_eq!(max_out, BigUint::from(500_000_000_000u64));

        let res = state.get_amount_out(u256_to_biguint(wad(500_001)), &usds(), &usdc());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn wrapper_unbinding_escrows_leave_mirror_limits() {
        let state = wrapper_state(JoinEscrows { dai: wad(3_000_000), usds: wad(9_000_000) });
        let (max_in, max_out) = state
            .get_limits(usds().address, usdc().address)
            .unwrap();
        // Pocket-bounded, as without escrow tracking.
        assert_eq!(max_out, BigUint::from(2_000_000_000_000u64));
        assert_eq!(max_in, u256_to_biguint(wad(2_000_000)));
        let (_, max_out) = state
            .get_limits(usdc().address, usds().address)
            .unwrap();
        // Buffer-bounded, as without escrow tracking.
        assert_eq!(max_out, u256_to_biguint(wad(1_000_000)));
    }

    #[test]
    fn wrapper_zero_usds_escrow_zeroes_buy_gem_limits() {
        // The pre-launch window: no USDS exists, so nothing can be burned.
        let state = wrapper_state(JoinEscrows { dai: wad(3_000_000), usds: U256::ZERO });
        let (max_in, max_out) = state
            .get_limits(usds().address, usdc().address)
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn wrapper_swap_moves_escrows_in_new_state() {
        let escrows = JoinEscrows { dai: wad(3_000_000), usds: wad(9_000_000) };
        let state = wrapper_state(escrows);

        // sellGem: the DAI payout is burned into freshly minted USDS.
        let res = state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &usds())
            .unwrap();
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<SkyState>()
            .unwrap();
        let new_escrows = new_state.escrows.unwrap();
        assert_eq!(new_escrows.dai, escrows.dai - U256::from(WAD));
        assert_eq!(new_escrows.usds, escrows.usds + U256::from(WAD));

        // buyGem: the full USDS input is burned back into DAI.
        let res = state
            .get_amount_out(BigUint::from(WAD), &usds(), &usdc())
            .unwrap();
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<SkyState>()
            .unwrap();
        let new_escrows = new_state.escrows.unwrap();
        assert_eq!(new_escrows.usds, escrows.usds - U256::from(WAD));
        assert_eq!(new_escrows.dai, escrows.dai + U256::from(WAD));
    }

    #[test]
    fn delta_transition_updates_escrows() {
        let mut state = wrapper_state(JoinEscrows { dai: wad(3_000_000), usds: wad(9_000_000) });
        let delta = ProtocolStateDelta {
            component_id: state.component_id.clone(),
            updated_attributes: HashMap::from([
                ("dai_escrow".to_string(), Bytes::from(wad(5).to_be_bytes_vec())),
                ("usds_escrow".to_string(), Bytes::from(wad(6).to_be_bytes_vec())),
            ]),
            deleted_attributes: Default::default(),
        };
        let balances =
            Balances { component_balances: HashMap::new(), account_balances: HashMap::new() };
        state
            .delta_transition(delta, &HashMap::new(), &balances)
            .unwrap();
        assert_eq!(state.escrows.unwrap(), JoinEscrows { dai: wad(5), usds: wad(6) });
    }

    /// Price of `price` DAI-wei per 1 USDC (1e6 raw).
    fn usdc_dai_price(dai_wei: u128) -> Price {
        Price::new(BigUint::from(dai_wei), BigUint::from(1_000_000u32))
    }

    #[test]
    fn query_pool_swap_trade_limit_is_all_or_nothing() {
        // tin = 0.1%: flat USDC -> DAI execution price of 0.999.
        let state = psm_state(U256::from(WAD / 1000), U256::ZERO);

        // A limit below the flat price buys the full capacity.
        let swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                usdc(),
                dai(),
                SwapConstraint::TradeLimitPrice {
                    limit: usdc_dai_price(990_000_000_000_000_000), // 0.99
                    tolerance: 0.0,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        let (max_in, max_out) = state
            .get_limits(usdc().address, dai().address)
            .unwrap();
        assert_eq!(swap.amount_in(), &max_in);
        assert_eq!(swap.amount_out(), &max_out);

        // A limit above the flat price cannot be met at any size.
        let swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                usdc(),
                dai(),
                SwapConstraint::TradeLimitPrice {
                    limit: usdc_dai_price(999_500_000_000_000_000), // 0.9995
                    tolerance: 0.0001,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        assert_eq!(swap.amount_in(), &BigUint::from(0u8));
        assert_eq!(swap.amount_out(), &BigUint::from(0u8));

        // The limit is a hard floor: even a generous tolerance must not admit
        // an execution price below it.
        let swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                usdc(),
                dai(),
                SwapConstraint::TradeLimitPrice {
                    limit: usdc_dai_price(999_500_000_000_000_000), // 0.9995
                    tolerance: 0.01,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        assert_eq!(swap.amount_in(), &BigUint::from(0u8));
        assert_eq!(swap.amount_out(), &BigUint::from(0u8));
    }

    #[test]
    fn query_pool_swap_target_price_only_satisfiable_at_spot() {
        // tout = 0.2%: spot buy price of USDC is 1.002 DAI.
        let state = psm_state(U256::ZERO, U256::from(2 * (WAD / 1000)));

        let swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                usdc(),
                dai(),
                SwapConstraint::PoolTargetPrice {
                    target: usdc_dai_price(1_002_000_000_000_000_000), // 1.002 == spot
                    tolerance: 1e-9,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        assert_eq!(swap.amount_in(), &BigUint::from(0u8));

        let res = state.query_pool_swap(&QueryPoolSwapParams::new(
            usdc(),
            dai(),
            SwapConstraint::PoolTargetPrice {
                target: usdc_dai_price(1_010_000_000_000_000_000), // 1.01 != spot
                tolerance: 1e-9,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));
        assert!(matches!(res, Err(SimulationError::InvalidInput(_, _))));
    }

    #[test]
    fn delta_transition_updates_fees_and_balances() {
        let mut state = psm_state(U256::ZERO, U256::ZERO);
        let delta = ProtocolStateDelta {
            component_id: state.component_id.clone(),
            updated_attributes: HashMap::from([(
                "tin".to_string(),
                Bytes::from(U256::from(WAD / 100).to_be_bytes_vec()),
            )]),
            deleted_attributes: Default::default(),
        };
        let balances = Balances {
            component_balances: HashMap::from([(
                state.component_id.clone(),
                HashMap::from([(dai().address, Bytes::from(wad(500_000).to_be_bytes_vec()))]),
            )]),
            account_balances: HashMap::new(),
        };
        state
            .delta_transition(delta, &HashMap::new(), &balances)
            .unwrap();
        assert_eq!(state.tin, U256::from(WAD / 100));
        assert_eq!(state.stable_balance, wad(500_000));
    }

    #[test]
    fn invalid_pair_errors() {
        let state = psm_state(U256::ZERO, U256::ZERO);
        assert!(state
            .get_amount_out(BigUint::from(1u64), &usds(), &dai())
            .is_err());
    }
}
