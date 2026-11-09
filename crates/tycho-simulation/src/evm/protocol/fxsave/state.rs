use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
use num_bigint::BigUint;
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
        fxsave::vm::{self, VaultReadings},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    },
};

/// `fxSP.deposit` followed by `fxSAVE.deposit`, measured at 495k through the router on a
/// mainnet fork (block 26,353,817). About 246k of it is fxSAVE staking its idle fxSP into the
/// gauge, which it does once that balance reaches its threshold (100 fxSP), so on almost every
/// deposit.
const DEPOSIT_GAS: u64 = 500_000;

/// fxSP's fixed-point unit.
const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The largest deposit quoted, in whole tokens of either input. Deposits are uncapped; fxSP's share
/// math (`amountUSD * totalSupply`) overflows `uint256` and reverts long before `2^128` USDC units,
/// so the bound has to sit well below it.
const MAX_DEPOSIT_WHOLE_TOKENS: u64 = 1_000_000_000_000_000_000;

/// Deposits into fxSAVE: USDC or fxUSD into fxSP (f(x) Protocol's stability pool share), then the
/// fxSP into fxSAVE. Both steps are linear, so a quote is exact at any size.
///
/// Leaving fxSAVE redeems fxSP for fxUSD and USDC after a cooldown, so only the two directions
/// into fxSAVE are quoted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxSaveState {
    vault: Bytes,
    base_pool: Bytes,
    vault_token: Token,
    stable_token: Token,
    yield_token: Token,
    total_yield_token: U256,
    total_stable_token: U256,
    base_pool_supply: U256,
    /// USDC price scaled to 18 decimals of USDC; `None` while the oracle reverts.
    stable_price: Option<U256>,
    stable_depeg_price: U256,
    vault_assets: U256,
    vault_supply: U256,
}

impl FxSaveState {
    /// Builds the state from the vault's readings.
    ///
    /// Fails when `stable_token` or `yield_token` is not the token fxSP reports.
    pub fn new(
        vault_token: Token,
        base_pool: Bytes,
        stable_token: Token,
        yield_token: Token,
        readings: &VaultReadings,
    ) -> Result<Self, SimulationError> {
        if stable_token.address != readings.stable_token ||
            yield_token.address != readings.yield_token
        {
            return Err(SimulationError::FatalError(format!(
                "fxSP {base_pool} takes {} and {}, not {} and {}",
                readings.stable_token,
                readings.yield_token,
                stable_token.address,
                yield_token.address
            )));
        }
        let mut state = Self {
            vault: vault_token.address.clone(),
            base_pool,
            vault_token,
            stable_token,
            yield_token,
            total_yield_token: U256::ZERO,
            total_stable_token: U256::ZERO,
            base_pool_supply: U256::ZERO,
            stable_price: None,
            stable_depeg_price: U256::ZERO,
            vault_assets: U256::ZERO,
            vault_supply: U256::ZERO,
        };
        state.apply_readings(readings);
        Ok(state)
    }

    fn apply_readings(&mut self, readings: &VaultReadings) {
        self.total_yield_token = readings.total_yield_token;
        self.total_stable_token = readings.total_stable_token;
        self.base_pool_supply = readings.base_pool_supply;
        self.stable_price = readings.stable_price;
        self.stable_depeg_price = readings.stable_depeg_price;
        self.vault_assets = readings.vault_assets;
        self.vault_supply = readings.vault_supply;
    }

    /// USDC's price, when fxSP accepts deposits: the oracle answers and USDC is not below
    /// `stableDepegPrice`. fxSP checks this for both tokens.
    fn deposit_price(&self) -> Result<U256, SimulationError> {
        let Some(price) = self.stable_price else {
            return Err(SimulationError::RecoverableError(format!(
                "fxSP {} rejects deposits: its USDC price oracle is stale",
                self.base_pool
            )));
        };
        let scale = U256::from(10).pow(U256::from(18 - self.stable_token.decimals));
        if price < self.stable_depeg_price * scale {
            return Err(SimulationError::RecoverableError(format!(
                "fxSP {} rejects deposits: USDC price {price} is below the depeg price",
                self.base_pool
            )));
        }
        Ok(price)
    }

    /// The largest deposit of `token_in` quoted, in its smallest unit.
    fn max_deposit(&self, token_in: &Bytes) -> U256 {
        let decimals = if *token_in == self.stable_token.address {
            self.stable_token.decimals
        } else {
            self.yield_token.decimals
        };
        U256::from(MAX_DEPOSIT_WHOLE_TOKENS) * U256::from(10).pow(U256::from(decimals))
    }

    fn is_deposit(&self, token_in: &Bytes, token_out: &Bytes) -> bool {
        *token_out == self.vault &&
            (*token_in == self.stable_token.address || *token_in == self.yield_token.address)
    }

    /// Returns the fxSP and fxSAVE shares minted for `amount` of `token_in`, as
    /// `FxUSDBasePool._deposit` and OpenZeppelin's `ERC4626.previewDeposit` compute them.
    fn deposit(&self, token_in: &Bytes, amount: U256) -> Result<(U256, U256), SimulationError> {
        let price = self.deposit_price()?;
        let amount_usd = if *token_in == self.stable_token.address {
            mul_div(amount, price, PRECISION)?
        } else {
            amount
        };
        let base_pool_shares = if self.base_pool_supply.is_zero() {
            amount_usd
        } else {
            let stable_usd = mul_div(self.total_stable_token, price, PRECISION)?;
            let total_usd = self
                .total_yield_token
                .checked_add(stable_usd)
                .ok_or_else(|| overflow("fxSP total USD"))?;
            mul_div(amount_usd, self.base_pool_supply, total_usd)?
        };
        let vault_shares = mul_div(
            base_pool_shares,
            self.vault_supply + U256::ONE,
            self.vault_assets + U256::ONE,
        )?;
        Ok((base_pool_shares, vault_shares))
    }

    fn unsupported_pair(&self, token_in: &Bytes, token_out: &Bytes) -> SimulationError {
        SimulationError::InvalidInput(
            format!(
                "fxSAVE {} only takes USDC or fxUSD deposits, not {token_in} -> {token_out}",
                self.vault
            ),
            None,
        )
    }
}

/// `a * b / c` as Solidity computes it, failing where the contract would revert.
fn mul_div(a: U256, b: U256, c: U256) -> Result<U256, SimulationError> {
    a.checked_mul(b)
        .and_then(|product| product.checked_div(c))
        .ok_or_else(|| overflow("fxSP or fxSAVE share math"))
}

fn overflow(what: &str) -> SimulationError {
    SimulationError::InvalidInput(format!("{what} overflows or divides by zero"), None)
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

        let mut new_state = self.clone();
        if token_in.address == self.stable_token.address {
            new_state.total_stable_token += amount;
        } else {
            new_state.total_yield_token += amount;
        }
        new_state.base_pool_supply += base_pool_shares;
        new_state.vault_assets += base_pool_shares;
        new_state.vault_supply += vault_shares;

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(vault_shares),
            gas: BigUint::from(DEPOSIT_GAS),
            new_state: Box::new(new_state),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let tokens = [&self.stable_token.address, &self.yield_token.address, &self.vault];
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
        let readings = vm::read_vault(&engine, &self.vault, &self.base_pool)?;
        self.apply_readings(&readings);
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

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

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

    /// fxSP and fxSAVE at block 26,353,817.
    pub fn readings() -> VaultReadings {
        VaultReadings {
            stable_token: Bytes::from_str(USDC).unwrap(),
            yield_token: Bytes::from_str(FXUSD).unwrap(),
            total_yield_token: U256::from(53_403_819_603_874_249_470_787_312_u128),
            total_stable_token: U256::from(10_867_685_419_190_u64),
            base_pool_supply: U256::from(63_616_689_726_854_618_890_912_910_u128),
            stable_price: Some(U256::from(999_820_610_000_000_000_000_000_000_000_u128)),
            stable_depeg_price: U256::from(995_000_000_000_000_000_u64),
            vault_assets: U256::from(61_676_520_292_070_393_820_457_103_u128),
            vault_supply: U256::from(55_205_955_124_401_984_731_778_539_u128),
        }
    }

    fn state_with(readings: &VaultReadings) -> FxSaveState {
        FxSaveState::new(
            token(FXSAVE, "fxSAVE", 18),
            Bytes::from_str(FX_BASE_POOL).unwrap(),
            token(USDC, "USDC", 6),
            token(FXUSD, "fxUSD", 18),
            readings,
        )
        .unwrap()
    }

    fn amount_out(state: &FxSaveState, amount: u128, token_in: &Token) -> BigUint {
        state
            .get_amount_out(BigUint::from(amount), token_in, &token(FXSAVE, "fxSAVE", 18))
            .unwrap()
            .amount
    }

    #[test]
    fn usdc_deposit_matches_the_chain_preview() {
        let state = state_with(&readings());

        // fxSP.previewDeposit(USDC, 1e9) then fxSAVE.previewDeposit at block 26,353,817.
        assert_eq!(
            amount_out(&state, 1_000_000_000, &token(USDC, "USDC", 6)),
            BigUint::from(885_837_212_934_908_530_177_u128)
        );
    }

    #[test]
    fn fxusd_deposit_matches_the_chain_preview() {
        let state = state_with(&readings());

        assert_eq!(
            amount_out(&state, 1_000_000_000_000_000_000_000, &token(FXUSD, "fxUSD", 18)),
            BigUint::from(885_996_151_784_577_165_475_u128)
        );
    }

    #[test]
    fn deposit_returns_a_state_holding_it() {
        let state = state_with(&readings());
        let usdc = token(USDC, "USDC", 6);

        let result = state
            .get_amount_out(BigUint::from(1_000_000_000u64), &usdc, &token(FXSAVE, "fxSAVE", 18))
            .unwrap();

        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<FxSaveState>()
            .unwrap();
        assert_eq!(
            new_state.total_stable_token,
            state.total_stable_token + U256::from(1_000_000_000u64)
        );
        assert_eq!(new_state.vault_supply, state.vault_supply + biguint_to_u256(&result.amount));
        assert_eq!(
            new_state.vault_assets - state.vault_assets,
            new_state.base_pool_supply - state.base_pool_supply
        );
    }

    #[test]
    fn redeeming_fxsave_is_not_quoted() {
        let state = state_with(&readings());
        let fxsave = token(FXSAVE, "fxSAVE", 18);
        let usdc = token(USDC, "USDC", 6);

        assert!(matches!(
            state.get_amount_out(BigUint::from(1u64), &fxsave, &usdc),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert_eq!(
            state
                .get_limits(fxsave.address, usdc.address.clone())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        assert_eq!(
            state
                .get_limits(usdc.address, Bytes::from_str(FXUSD).unwrap())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn foreign_tokens_are_rejected() {
        let state = state_with(&readings());
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        assert!(state
            .get_limits(dai, Bytes::from_str(FXSAVE).unwrap())
            .is_err());
    }

    #[test]
    fn stale_oracle_closes_deposits() {
        let state = state_with(&VaultReadings { stable_price: None, ..readings() });
        let usdc = token(USDC, "USDC", 6);

        assert!(matches!(
            state.get_amount_out(BigUint::from(1_000_000u64), &usdc, &token(FXSAVE, "fxSAVE", 18)),
            Err(SimulationError::RecoverableError(_))
        ));
        assert_eq!(
            state
                .get_limits(usdc.address, Bytes::from_str(FXSAVE).unwrap())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn depegged_usdc_closes_fxusd_deposits_too() {
        let depegged = U256::from(994_999_999_999_999_999_u64) * U256::from(1_000_000_000_000u64);
        let state = state_with(&VaultReadings { stable_price: Some(depegged), ..readings() });

        assert!(state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u64),
                &token(FXUSD, "fxUSD", 18),
                &token(FXSAVE, "fxSAVE", 18),
            )
            .is_err());
    }

    #[test]
    fn usdc_at_the_depeg_price_still_deposits() {
        let at_depeg = U256::from(995_000_000_000_000_000_u64) * U256::from(1_000_000_000_000u64);
        let state = state_with(&VaultReadings { stable_price: Some(at_depeg), ..readings() });

        assert!(state
            .get_amount_out(
                BigUint::from(1_000_000u64),
                &token(USDC, "USDC", 6),
                &token(FXSAVE, "fxSAVE", 18),
            )
            .is_ok());
    }

    #[test]
    fn zero_and_oversized_deposits_are_rejected() {
        let state = state_with(&readings());
        let usdc = token(USDC, "USDC", 6);
        let fxsave = token(FXSAVE, "fxSAVE", 18);

        assert!(state
            .get_amount_out(BigUint::ZERO, &usdc, &fxsave)
            .is_err());
        assert!(state
            .get_amount_out(BigUint::from(10u32).pow(24) + 1u32, &usdc, &fxsave)
            .is_err());
    }

    #[test]
    fn limits_quote_a_quintillion_tokens_of_either_input() {
        let state = state_with(&readings());

        for (address, symbol, decimals) in [(USDC, "USDC", 6), (FXUSD, "fxUSD", 18)] {
            let (max_in, max_out) = state
                .get_limits(Bytes::from_str(address).unwrap(), Bytes::from_str(FXSAVE).unwrap())
                .unwrap();

            let expected_in = BigUint::from(10u32).pow(18 + decimals);
            assert_eq!(max_in, expected_in, "{symbol}");
            let quoted = state
                .get_amount_out(
                    expected_in,
                    &token(address, symbol, decimals),
                    &token(FXSAVE, "fxSAVE", 18),
                )
                .unwrap()
                .amount;
            assert_eq!(max_out, quoted, "{symbol}");
        }
    }

    #[test]
    fn share_math_overflow_is_rejected_not_wrapped() {
        let state = state_with(&VaultReadings {
            base_pool_supply: U256::MAX / U256::from(2),
            ..readings()
        });

        assert!(matches!(
            state.get_amount_out(
                BigUint::from(1_000_000_000u64),
                &token(USDC, "USDC", 6),
                &token(FXSAVE, "fxSAVE", 18),
            ),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }

    #[test]
    fn mismatched_tokens_are_rejected() {
        let result = FxSaveState::new(
            token(FXSAVE, "fxSAVE", 18),
            Bytes::from_str(FX_BASE_POOL).unwrap(),
            token(FXUSD, "fxUSD", 18),
            token(USDC, "USDC", 6),
            &readings(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn spot_price_is_one_unit_deposited() {
        let state = state_with(&readings());

        let price = state
            .spot_price(&token(USDC, "USDC", 6), &token(FXSAVE, "fxSAVE", 18))
            .unwrap();

        // 1 USDC buys about 0.8858 fxSAVE at block 26,353,817.
        assert!((price - 0.885_837).abs() < 1e-6, "{price}");
    }
}
