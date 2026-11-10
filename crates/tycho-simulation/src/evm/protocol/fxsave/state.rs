use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address as AlloyAddress, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        fxsave::vm::{self, VaultReadings},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::solidity_math,
    },
};

/// Gas of the executor's leg, `fxSP.deposit` then `fxSAVE.deposit`, when fxSAVE keeps the fxSP
/// idle: `FxSaveExecutor.swap` used 302k for USDC and 306k for fxUSD before refunds on a mainnet
/// fork at block 26,353,817. The router's transfers and approval around the leg are left out:
/// `estimate_gas_usage` adds them.
const DEPOSIT_GAS: u64 = 306_000;

/// Gas fxSAVE adds when a deposit brings its idle fxSP to `getThreshold` and it stakes all of it
/// into the gauge (`SavingFxUSD._deposit`): 246k for either token, measured as above.
const GAUGE_STAKE_GAS: u64 = 247_000;

/// The fxSP (`FxUSDBasePool`) implementation the deposit math and depeg gate port.
const BASE_POOL_IMPLEMENTATION: AlloyAddress = address!("9cfefd90d4c8428d4cbac9baaa6d52c6ba7897f9");

/// The fxSAVE (`SavingFxUSD`, OpenZeppelin `ERC4626`) implementation the share math and gauge
/// staking port.
const VAULT_IMPLEMENTATION: AlloyAddress = address!("e4031e271809d20074e4bef1caeefec5f710e8a6");

/// fxSP's fixed-point unit.
const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The largest deposit quoted, in whole tokens of either input. Deposits are uncapped, but fxSP's
/// share math (`amountUSD * totalSupply`) overflows `uint256` and reverts: at fxSP's supply of
/// about 6.4e25 shares, from about 1.8e39 USDC units, or 1.8e33 whole tokens of either input.
/// This is a conservative cap far below that.
const MAX_DEPOSIT_WHOLE_TOKENS: u64 = 1_000_000_000_000_000_000;

/// Deposits into fxSAVE: USDC or fxUSD into fxSP (f(x) Protocol's stability pool share), then the
/// fxSP into fxSAVE. Both steps are linear, so a quote is exact at any size.
///
/// Leaving fxSAVE redeems fxSP for fxUSD and USDC after a cooldown, so only the two directions
/// into fxSAVE are quoted.
///
/// The math is a port of fxSP's and fxSAVE's current implementations, which both proxies can
/// replace: fxSP's has changed three times since block 22,000,000. An upgrade can add a fee, a cap,
/// a pause or a new valuation the port would not see, so while either proxy points elsewhere no
/// deposit is quoted, until the port is checked against the new code and its address pinned here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxSaveState {
    base_pool: Bytes,
    vault_token: Token,
    stable_token: Token,
    yield_token: Token,
    readings: VaultReadings,
}

impl FxSaveState {
    /// Builds the state from fxSP's two tokens and the vault's readings.
    pub fn new(
        vault_token: Token,
        base_pool: Bytes,
        stable_token: Token,
        yield_token: Token,
        readings: VaultReadings,
    ) -> Self {
        Self { base_pool, vault_token, stable_token, yield_token, readings }
    }

    /// Fails when either proxy delegates to an implementation the math was not ported from.
    fn check_implementations(&self) -> Result<(), SimulationError> {
        let proxies = [
            (
                "fxSP",
                &self.base_pool,
                self.readings.base_pool_implementation,
                BASE_POOL_IMPLEMENTATION,
            ),
            (
                "fxSAVE",
                &self.vault_token.address,
                self.readings.vault_implementation,
                VAULT_IMPLEMENTATION,
            ),
        ];
        for (name, proxy, implementation, ported) in proxies {
            if implementation != ported {
                return Err(SimulationError::RecoverableError(format!(
                    "{name} {proxy} delegates to implementation {implementation}, not {ported} \
                     whose deposit math is ported: deposits are not quoted until the port is \
                     checked against it"
                )));
            }
        }
        Ok(())
    }

    /// USDC's price, when fxSP accepts deposits: the oracle answers and USDC is not below
    /// `stableDepegPrice`. fxSP checks this for both tokens. Also fails while either proxy runs an
    /// implementation the math was not ported from.
    fn deposit_price(&self) -> Result<U256, SimulationError> {
        self.check_implementations()?;
        let Some(price) = self.readings.stable_price else {
            return Err(SimulationError::RecoverableError(format!(
                "fxSP {} rejects deposits: its USDC price oracle reverts",
                self.base_pool
            )));
        };
        let scale = U256::from(10).pow(U256::from(18 - self.stable_token.decimals));
        let depeg_price = self
            .readings
            .stable_depeg_price
            .checked_mul(scale)
            .ok_or_else(|| overflow("fxSP stableDepegPrice * stableTokenScale"))?;
        if price < depeg_price {
            return Err(SimulationError::RecoverableError(format!(
                "fxSP {} rejects deposits: USDC price {price} is below the depeg price",
                self.base_pool
            )));
        }
        Ok(price)
    }

    fn is_stable_token(&self, token: &Bytes) -> bool {
        *token == self.stable_token.address
    }

    /// The largest deposit of `token_in` quoted, in its smallest unit.
    fn max_deposit(&self, token_in: &Bytes) -> U256 {
        let decimals = if self.is_stable_token(token_in) {
            self.stable_token.decimals
        } else {
            self.yield_token.decimals
        };
        U256::from(MAX_DEPOSIT_WHOLE_TOKENS) * U256::from(10).pow(U256::from(decimals))
    }

    fn is_deposit(&self, token_in: &Bytes, token_out: &Bytes) -> bool {
        *token_out == self.vault_token.address &&
            (self.is_stable_token(token_in) || *token_in == self.yield_token.address)
    }

    /// Returns the fxSP and fxSAVE shares minted for `amount` of `token_in`, as
    /// `FxUSDBasePool._deposit` and OpenZeppelin's `ERC4626.previewDeposit` compute them.
    fn deposit(&self, token_in: &Bytes, amount: U256) -> Result<(U256, U256), SimulationError> {
        let price = self.deposit_price()?;
        let readings = &self.readings;
        let amount_usd = if self.is_stable_token(token_in) {
            checked_mul_div(amount, price, PRECISION, "fxSP amountUSD")?
        } else {
            amount
        };
        let base_pool_shares = if readings.base_pool_supply.is_zero() {
            amount_usd
        } else {
            let stable_usd =
                checked_mul_div(readings.total_stable_token, price, PRECISION, "fxSP stable USD")?;
            let total_usd = checked_add(readings.total_yield_token, stable_usd, "fxSP totalUSD")?;
            checked_mul_div(amount_usd, readings.base_pool_supply, total_usd, "fxSP shares")?
        };
        let vault_shares = solidity_math::mul_div(
            base_pool_shares,
            checked_add(readings.vault_supply, U256::ONE, "fxSAVE totalSupply + 1")?,
            checked_add(readings.vault_assets, U256::ONE, "fxSAVE totalAssets + 1")?,
        )
        .map_err(|e| {
            SimulationError::InvalidInput(format!("fxSAVE previewDeposit failed: {e}"), None)
        })?;
        Ok((base_pool_shares, vault_shares))
    }

    /// The state after a deposit minting `base_pool_shares` and `vault_shares`, and whether fxSAVE
    /// then stakes its idle fxSP into the gauge.
    fn after_deposit(
        &self,
        token_in: &Bytes,
        amount: U256,
        base_pool_shares: U256,
        vault_shares: U256,
    ) -> Result<(Self, bool), SimulationError> {
        let mut new_state = self.clone();
        let readings = &mut new_state.readings;
        if self.is_stable_token(token_in) {
            readings.total_stable_token =
                checked_add(readings.total_stable_token, amount, "fxSP totalStableToken")?;
        } else {
            readings.total_yield_token =
                checked_add(readings.total_yield_token, amount, "fxSP totalYieldToken")?;
        }
        readings.base_pool_supply =
            checked_add(readings.base_pool_supply, base_pool_shares, "fxSP totalSupply")?;
        readings.vault_assets =
            checked_add(readings.vault_assets, base_pool_shares, "fxSAVE totalAssets")?;
        readings.vault_supply =
            checked_add(readings.vault_supply, vault_shares, "fxSAVE totalSupply")?;
        let idle = checked_add(readings.vault_idle_assets, base_pool_shares, "fxSAVE idle fxSP")?;
        let stakes = idle >= readings.stake_threshold;
        // Staking moves the idle fxSP into the gauge; `totalAssets` counts both.
        readings.vault_idle_assets = if stakes { U256::ZERO } else { idle };
        Ok((new_state, stakes))
    }

    fn unsupported_pair(&self, token_in: &Bytes, token_out: &Bytes) -> SimulationError {
        SimulationError::InvalidInput(
            format!(
                "fxSAVE {} only takes USDC or fxUSD deposits, not {token_in} -> {token_out}",
                self.vault_token.address
            ),
            None,
        )
    }
}

/// `a * b / c` with Solidity 0.8's checked arithmetic, failing where fxSP would revert.
fn checked_mul_div(a: U256, b: U256, c: U256, operation: &str) -> Result<U256, SimulationError> {
    let product = a
        .checked_mul(b)
        .ok_or_else(|| overflow(operation))?;
    product
        .checked_div(c)
        .ok_or_else(|| SimulationError::InvalidInput(format!("{operation} divides by zero"), None))
}

fn checked_add(a: U256, b: U256, operation: &str) -> Result<U256, SimulationError> {
    a.checked_add(b)
        .ok_or_else(|| overflow(operation))
}

fn overflow(operation: &str) -> SimulationError {
    SimulationError::InvalidInput(format!("{operation} overflows uint256"), None)
}

#[typetag::serde]
impl ProtocolSim for FxSaveState {
    fn fee(&self) -> f64 {
        0f64
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if !self.is_deposit(&base.address, &quote.address) {
            return Err(self.unsupported_pair(&base.address, &quote.address));
        }
        let unit = U256::from(10).pow(U256::from(base.decimals));
        let (_, vault_shares) = self.deposit(&base.address, unit)?;
        let vault_unit = U256::from(10).pow(U256::from(self.vault_token.decimals));
        Ok(u256_to_f64(vault_shares)? / u256_to_f64(vault_unit)?)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if !self.is_deposit(&token_in.address, &token_out.address) {
            return Err(self.unsupported_pair(&token_in.address, &token_out.address));
        }
        let amount = biguint_to_u256(&amount_in);
        if amount.is_zero() {
            return Err(SimulationError::InvalidInput(
                "fxSP rejects a zero deposit".to_string(),
                None,
            ));
        }
        let max_deposit = self.max_deposit(&token_in.address);
        if amount > max_deposit {
            return Err(SimulationError::InvalidInput(
                format!("Deposit of {amount} exceeds the largest quoted deposit {max_deposit}"),
                None,
            ));
        }
        let (base_pool_shares, vault_shares) = self.deposit(&token_in.address, amount)?;
        let (new_state, stakes) =
            self.after_deposit(&token_in.address, amount, base_pool_shares, vault_shares)?;
        let gas = if stakes { DEPOSIT_GAS + GAUGE_STAKE_GAS } else { DEPOSIT_GAS };

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(vault_shares),
            gas: BigUint::from(gas),
            new_state: Box::new(new_state),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let tokens =
            [&self.stable_token.address, &self.yield_token.address, &self.vault_token.address];
        if !tokens.contains(&&sell_token) ||
            !tokens.contains(&&buy_token) ||
            sell_token == buy_token
        {
            return Err(self.unsupported_pair(&sell_token, &buy_token));
        }
        if !self.is_deposit(&sell_token, &buy_token) || self.deposit_price().is_err() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let max_in = self.max_deposit(&sell_token);
        let (_, max_out) = self.deposit(&sell_token, max_in)?;
        Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let engine = create_engine(SHARED_TYCHO_DB.clone(), false)?;
        self.readings = vm::read_vault(&engine, &self.vault_token.address, &self.base_pool)?;
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
            .downcast_ref::<FxSaveState>()
            .is_some_and(|other| self == other)
    }

    /// A deposit's price does not depend on its size, so the generic search, which needs the
    /// price to fall as the amount grows, cannot bracket it. Both constraints are answered from
    /// the rate of the largest quoted deposit, compared exactly as `amount_out / amount_in`: a
    /// deposit never moves the price, so the whole limit is deposited when that rate meets the
    /// limit or target price, and nothing otherwise (also while deposits are closed).
    ///
    /// `tolerance`, `min_amount_in` and `max_amount_in` are ignored, as in the generic helper.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let token_in = params.token_in();
        let token_out = params.token_out();
        let price = match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { limit, .. } => limit,
            SwapConstraint::PoolTargetPrice { target, .. } => target,
        };
        let (max_in, max_out) =
            self.get_limits(token_in.address.clone(), token_out.address.clone())?;
        if max_in == BigUint::ZERO || &max_out * &price.denominator < &price.numerator * &max_in {
            return Ok(PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None));
        }
        let result = self.get_amount_out(max_in.clone(), token_in, token_out)?;
        Ok(PoolSwap::new(max_in, result.amount, result.new_state, None))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::{models::Chain, simulation::protocol_sim::Price};

    use super::*;

    pub const FXSAVE: &str = "0x7743e50f534a7f9f1791dde7dcd89f7783eefc39";
    pub const FX_BASE_POOL: &str = "0x65c9a641afceb9c0e6034e558a319488fa0fa3be";
    pub const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    pub const FXUSD: &str = "0x085780639cc2cacd35e474e71f4d000e2405d8f6";

    pub fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(100_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        token(USDC, "USDC", 6)
    }

    fn fxusd() -> Token {
        token(FXUSD, "fxUSD", 18)
    }

    fn fxsave() -> Token {
        token(FXSAVE, "fxSAVE", 18)
    }

    /// fxSP and fxSAVE at block 26,353,817.
    pub fn readings() -> VaultReadings {
        VaultReadings {
            base_pool_implementation: address!("9cfefd90d4c8428d4cbac9baaa6d52c6ba7897f9"),
            vault_implementation: address!("e4031e271809d20074e4bef1caeefec5f710e8a6"),
            total_yield_token: U256::from(53_403_819_603_874_249_470_787_312_u128),
            total_stable_token: U256::from(10_867_685_419_190_u64),
            base_pool_supply: U256::from(63_616_689_726_854_618_890_912_910_u128),
            stable_price: Some(U256::from(999_820_610_000_000_000_000_000_000_000_u128)),
            stable_depeg_price: U256::from(995_000_000_000_000_000_u64),
            vault_assets: U256::from(61_676_520_292_070_393_820_457_103_u128),
            vault_supply: U256::from(55_205_955_124_401_984_731_778_539_u128),
            vault_idle_assets: U256::ZERO,
            stake_threshold: U256::from(100_000_000_000_000_000_000_u128),
        }
    }

    fn state_with(readings: VaultReadings) -> FxSaveState {
        FxSaveState::new(
            fxsave(),
            Bytes::from_str(FX_BASE_POOL).unwrap(),
            usdc(),
            fxusd(),
            readings,
        )
    }

    fn deposit(state: &FxSaveState, amount: u128, token_in: &Token) -> GetAmountOutResult {
        state
            .get_amount_out(BigUint::from(amount), token_in, &fxsave())
            .unwrap()
    }

    fn returned_state(result: &GetAmountOutResult) -> &FxSaveState {
        result
            .new_state
            .as_any()
            .downcast_ref::<FxSaveState>()
            .unwrap()
    }

    /// fxSP and fxSAVE shares minted by `fxSP.previewDeposit` then `fxSAVE.previewDeposit` at
    /// block 26,353,817.
    const USDC_1000_FXSP: u128 = 989_664_189_595_756_090_058;
    const USDC_1000_FXSAVE: u128 = 885_837_212_934_908_530_177;
    const FXUSD_1000_FXSP: u128 = 989_841_757_308_599_679_755;
    const FXUSD_1000_FXSAVE: u128 = 885_996_151_784_577_165_475;
    const USDC_10_FXSP: u128 = 9_896_641_895_957_560_900;
    const USDC_10_FXSAVE: u128 = 8_858_372_129_349_085_301;
    /// The same for the largest quoted USDC deposit, 1e18 USDC.
    const USDC_MAX_IN: u128 = 1_000_000_000_000_000_000_000_000;
    const USDC_MAX_FXSAVE: u128 = 885_837_212_934_908_530_178_367_802_461_475_189;

    #[test]
    fn usdc_deposit_matches_the_chain_and_returns_the_state_holding_it() {
        let state = state_with(readings());

        let result = deposit(&state, 1_000_000_000, &usdc());

        assert_eq!(result.amount, BigUint::from(USDC_1000_FXSAVE));
        let fxsp = U256::from(USDC_1000_FXSP);
        let expected = state_with(VaultReadings {
            total_stable_token: readings().total_stable_token + U256::from(1_000_000_000u64),
            base_pool_supply: readings().base_pool_supply + fxsp,
            vault_assets: readings().vault_assets + fxsp,
            vault_supply: readings().vault_supply + U256::from(USDC_1000_FXSAVE),
            // 989 fxSP reach the 100 fxSP threshold, so fxSAVE stakes them.
            vault_idle_assets: U256::ZERO,
            ..readings()
        });
        assert_eq!(returned_state(&result), &expected);
        assert_eq!(result.gas, BigUint::from(553_000_u64));
    }

    #[test]
    fn fxusd_deposit_matches_the_chain_and_returns_the_state_holding_it() {
        let state = state_with(readings());

        let result = deposit(&state, 1_000_000_000_000_000_000_000, &fxusd());

        assert_eq!(result.amount, BigUint::from(FXUSD_1000_FXSAVE));
        let fxsp = U256::from(FXUSD_1000_FXSP);
        let expected = state_with(VaultReadings {
            total_yield_token: readings().total_yield_token +
                U256::from(1_000_000_000_000_000_000_000_u128),
            base_pool_supply: readings().base_pool_supply + fxsp,
            vault_assets: readings().vault_assets + fxsp,
            vault_supply: readings().vault_supply + U256::from(FXUSD_1000_FXSAVE),
            vault_idle_assets: U256::ZERO,
            ..readings()
        });
        assert_eq!(returned_state(&result), &expected);
    }

    #[rstest]
    #[case::idle_stays_below_the_threshold(0, USDC_10_FXSP, 306_000)]
    #[case::one_unit_short_of_the_threshold(
        90_103_358_104_042_439_099,
        90_103_358_104_042_439_099 + USDC_10_FXSP,
        306_000
    )]
    #[case::idle_reaches_the_threshold(
        90_103_358_104_042_439_100,
        0,
        553_000
    )]
    fn gauge_stake_is_charged_when_idle_fxsp_reaches_the_threshold(
        #[case] idle_before: u128,
        #[case] idle_after: u128,
        #[case] gas: u64,
    ) {
        let state =
            state_with(VaultReadings { vault_idle_assets: U256::from(idle_before), ..readings() });

        let result = deposit(&state, 10_000_000, &usdc());

        assert_eq!(result.amount, BigUint::from(USDC_10_FXSAVE));
        assert_eq!(result.gas, BigUint::from(gas));
        let new_readings = &returned_state(&result).readings;
        assert_eq!(new_readings.vault_idle_assets, U256::from(idle_after));
        assert_eq!(new_readings.vault_assets, readings().vault_assets + U256::from(USDC_10_FXSP));
    }

    #[test]
    fn redeeming_fxsave_is_not_quoted() {
        let state = state_with(readings());

        assert!(matches!(
            state.get_amount_out(BigUint::from(1u64), &fxsave(), &usdc()),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.spot_price(&fxsave(), &usdc()),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert_eq!(
            state
                .get_limits(fxsave().address, usdc().address)
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        assert_eq!(
            state
                .get_limits(usdc().address, fxusd().address)
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn foreign_tokens_are_rejected() {
        let state = state_with(readings());
        let dai = token("0x6b175474e89094c44da98b954eedeac495271d0f", "DAI", 18);

        assert!(matches!(
            state.get_limits(dai.address.clone(), fxsave().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.spot_price(&dai, &fxsave()),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }

    /// 1 wei of 18-decimal USDC price below `stableDepegPrice` (0.995).
    fn depegged() -> VaultReadings {
        let price = U256::from(994_999_999_999_999_999_u64) * U256::from(1_000_000_000_000u64);
        VaultReadings { stable_price: Some(price), ..readings() }
    }

    fn upgraded_base_pool() -> VaultReadings {
        VaultReadings {
            base_pool_implementation: address!("1111111111111111111111111111111111111111"),
            ..readings()
        }
    }

    fn upgraded_vault() -> VaultReadings {
        VaultReadings { vault_implementation: AlloyAddress::ZERO, ..readings() }
    }

    #[rstest]
    #[case::stale_oracle(VaultReadings { stable_price: None, ..readings() }, "oracle reverts")]
    #[case::depegged_usdc(depegged(), "below the depeg price")]
    #[case::upgraded_base_pool(
        upgraded_base_pool(),
        "fxSP 0x65c9a641afceb9c0e6034e558a319488fa0fa3be delegates to implementation \
         0x1111111111111111111111111111111111111111, not 0x9CFeFD90d4C8428D4CBaC9bAAA6D52C6BA7897f9"
    )]
    #[case::upgraded_vault(
        upgraded_vault(),
        "fxSAVE 0x7743e50f534a7f9f1791dde7dcd89f7783eefc39 delegates to implementation \
         0x0000000000000000000000000000000000000000, not 0xE4031e271809d20074E4bef1caeEfEc5f710e8A6"
    )]
    fn closed_deposits_are_refused_for_both_tokens(
        #[case] readings: VaultReadings,
        #[case] reason: &str,
    ) {
        let state = state_with(readings);

        for token_in in [usdc(), fxusd()] {
            let symbol = &token_in.symbol;
            assert!(
                matches!(
                    state.get_amount_out(BigUint::from(1_000_000u64), &token_in, &fxsave()),
                    Err(SimulationError::RecoverableError(message)) if message.contains(reason)
                ),
                "{symbol}"
            );
            assert!(
                matches!(
                    state.spot_price(&token_in, &fxsave()),
                    Err(SimulationError::RecoverableError(message)) if message.contains(reason)
                ),
                "{symbol}"
            );
            assert_eq!(
                state
                    .get_limits(token_in.address.clone(), fxsave().address)
                    .unwrap(),
                (BigUint::ZERO, BigUint::ZERO),
                "{symbol}"
            );
            let swap = state
                .query_pool_swap(&QueryPoolSwapParams::new(
                    token_in.clone(),
                    fxsave(),
                    trade_limit(1),
                ))
                .unwrap();
            assert_eq!(swap.amount_in(), &BigUint::ZERO, "{symbol}");
        }
    }

    #[test]
    fn usdc_at_the_depeg_price_still_deposits() {
        let at_depeg = U256::from(995_000_000_000_000_000_u64) * U256::from(1_000_000_000_000u64);
        let state = state_with(VaultReadings { stable_price: Some(at_depeg), ..readings() });

        assert!(state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &fxsave())
            .is_ok());
    }

    #[test]
    fn zero_and_oversized_deposits_are_rejected() {
        let state = state_with(readings());

        assert!(matches!(
            state.get_amount_out(BigUint::ZERO, &usdc(), &fxsave()),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.get_amount_out(BigUint::from(USDC_MAX_IN) + 1u32, &usdc(), &fxsave()),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }

    #[test]
    fn limits_quote_a_quintillion_tokens_of_either_input() {
        let state = state_with(readings());

        assert_eq!(
            state
                .get_limits(usdc().address, fxsave().address)
                .unwrap(),
            (BigUint::from(USDC_MAX_IN), BigUint::from(USDC_MAX_FXSAVE))
        );
        // fxSAVE.previewDeposit(fxSP.previewDeposit(fxUSD, 1e36)) at block 26,353,817.
        assert_eq!(
            state
                .get_limits(fxusd().address, fxsave().address)
                .unwrap(),
            (
                BigUint::from(10u32).pow(36),
                BigUint::from(885_996_151_784_577_165_476_082_556_911_359_518_u128)
            )
        );
    }

    #[test]
    fn share_math_overflow_is_rejected_not_wrapped() {
        let state =
            state_with(VaultReadings { base_pool_supply: U256::MAX / U256::from(2), ..readings() });

        assert!(matches!(
            state.get_amount_out(BigUint::from(1_000_000_000u64), &usdc(), &fxsave()),
            Err(SimulationError::InvalidInput(message, None))
                if message == "fxSP shares overflows uint256"
        ));
    }

    #[test]
    fn spot_price_is_one_unit_deposited() {
        let state = state_with(readings());

        let price = state
            .spot_price(&usdc(), &fxsave())
            .unwrap();

        // 1 USDC buys about 0.8858 fxSAVE at block 26,353,817.
        assert!((price - 0.885_837).abs() < 1e-6, "{price}");
    }

    /// A price of `numerator` fxSAVE units per 1e18 USDC.
    fn usdc_price(numerator: u128) -> Price {
        Price::new(BigUint::from(numerator), BigUint::from(USDC_MAX_IN))
    }

    fn trade_limit(numerator: u128) -> SwapConstraint {
        SwapConstraint::TradeLimitPrice {
            limit: usdc_price(numerator),
            tolerance: 0.0,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    fn pool_target(numerator: u128) -> SwapConstraint {
        SwapConstraint::PoolTargetPrice {
            target: usdc_price(numerator),
            tolerance: 0.0,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    #[rstest]
    #[case::trade_limit_below_the_rate(trade_limit(USDC_MAX_FXSAVE / 2), true)]
    #[case::trade_limit_at_the_rate(trade_limit(USDC_MAX_FXSAVE), true)]
    #[case::trade_limit_above_the_rate(trade_limit(USDC_MAX_FXSAVE + 1), false)]
    #[case::pool_target_below_the_rate(pool_target(USDC_MAX_FXSAVE / 2), true)]
    #[case::pool_target_at_the_rate(pool_target(USDC_MAX_FXSAVE), true)]
    #[case::pool_target_above_the_rate(pool_target(USDC_MAX_FXSAVE + 1), false)]
    fn price_query_deposits_the_whole_limit_or_nothing(
        #[case] constraint: SwapConstraint,
        #[case] fills: bool,
    ) {
        let state = state_with(readings());

        let swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(usdc(), fxsave(), constraint))
            .unwrap();

        let (amount_in, amount_out) = if fills {
            (BigUint::from(USDC_MAX_IN), BigUint::from(USDC_MAX_FXSAVE))
        } else {
            (BigUint::ZERO, BigUint::ZERO)
        };
        assert_eq!(swap.amount_in(), &amount_in);
        assert_eq!(swap.amount_out(), &amount_out);
    }
}
