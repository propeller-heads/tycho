//! The base pool behind a StableSwap NG metapool, for quoting its underlying swaps.
//!
//! A metapool pairs one coin with the LP token of a base pool. `exchange_underlying` trades that
//! coin against the base pool's own coins in one call: it swaps to or from the LP token inside the
//! metapool and redeems or mints that LP token in the base pool. Quoting it needs the base pool's
//! reserves and LP supply next to the metapool's own state.
use std::fmt::Debug;

use alloy::primitives::{Address as AlloyAddress, U256};
use revm::DatabaseRef;
use serde::{Deserialize, Serialize};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::curve::{
        adapter::{build_pool, CurveVariant},
        math::{
            swap::stableswap_ng::{calc_deposit, calc_withdraw_one_coin, LiquidityPool},
            Pool,
        },
        vm,
    },
    simulation::SimulationEngine,
};

/// A metapool's base pool, with the metapool coin indexes its underlying swaps go through.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct BasePool {
    address: Bytes,
    /// The base pool's coins, in its own index order.
    tokens: Vec<Bytes>,
    decimals: Vec<u8>,
    pool: Pool,
    /// Total supply of the base pool's LP token.
    total_supply: U256,
    /// Index, in the metapool, of the coin that is not the base pool's LP token.
    pub(super) meta_coin: usize,
    /// Index, in the metapool, of the base pool's LP token.
    pub(super) lp_coin: usize,
}

impl BasePool {
    /// Reads the base pool at `address` from `engine`.
    ///
    /// Returns a [`SimulationError`] if the pool does not answer the StableSwap NG getters.
    pub(super) fn read<D: EngineDatabaseInterface + Clone + Debug>(
        engine: &SimulationEngine<D>,
        address: Bytes,
        tokens: Vec<Bytes>,
        decimals: Vec<u8>,
        (meta_coin, lp_coin): (usize, usize),
    ) -> Result<Self, SimulationError>
    where
        <D as DatabaseRef>::Error: Debug,
        <D as EngineDatabaseInterface>::Error: Debug,
    {
        let (pool, total_supply) = read_state(engine, &address, &decimals)?;
        Ok(Self { address, tokens, decimals, pool, total_supply, meta_coin, lp_coin })
    }

    /// Re-reads the reserves and LP supply from `engine`.
    pub(super) fn refresh<D: EngineDatabaseInterface + Clone + Debug>(
        &mut self,
        engine: &SimulationEngine<D>,
    ) -> Result<(), SimulationError>
    where
        <D as DatabaseRef>::Error: Debug,
        <D as EngineDatabaseInterface>::Error: Debug,
    {
        let (pool, total_supply) = read_state(engine, &self.address, &self.decimals)?;
        self.pool = pool;
        self.total_supply = total_supply;
        Ok(())
    }

    pub(super) fn coin_index(&self, token: &Bytes) -> Option<usize> {
        self.tokens
            .iter()
            .position(|coin| coin == token)
    }

    pub(super) fn balance(&self, coin: usize) -> U256 {
        self.pool.balances()[coin]
    }

    fn liquidity(&self) -> Option<LiquidityPool<'_>> {
        Some(LiquidityPool {
            balances: self.pool.balances(),
            rates: self.pool.rates()?,
            amp: self.pool.amp(),
            fee: self.pool.fee()?,
            offpeg_fee_multiplier: self.pool.offpeg_fee_multiplier()?,
            total_supply: self.total_supply,
        })
    }

    /// Burns `lp_amount` for `coin`. Returns the amount paid and the base pool afterwards.
    ///
    /// The pool afterwards keeps the whole fee in its reserves, although the contract moves the
    /// admin's share out: the reserves it quotes later swaps from are high by that share.
    pub(super) fn withdraw(&self, lp_amount: U256, coin: usize) -> Option<(U256, Self)> {
        let amount = calc_withdraw_one_coin(&self.liquidity()?, lp_amount, coin)?;
        let mut after = self.clone();
        after
            .pool
            .set_balance(coin, self.balance(coin).checked_sub(amount)?)
            .ok()?;
        after.total_supply = self
            .total_supply
            .checked_sub(lp_amount)?;
        Some((amount, after))
    }

    /// Deposits `amount` of `coin`. Returns the LP tokens minted and the base pool afterwards.
    ///
    /// The pool afterwards keeps the whole fee in its reserves, as in [`Self::withdraw`].
    pub(super) fn deposit(&self, amount: U256, coin: usize) -> Option<(U256, Self)> {
        let mut amounts = vec![U256::ZERO; self.tokens.len()];
        *amounts.get_mut(coin)? = amount;
        let minted = calc_deposit(&self.liquidity()?, &amounts)?;
        let mut after = self.clone();
        after
            .pool
            .set_balance(coin, self.balance(coin).checked_add(amount)?)
            .ok()?;
        after.total_supply = self.total_supply.checked_add(minted)?;
        Some((minted, after))
    }

    #[cfg(test)]
    pub(super) fn new(
        tokens: Vec<Bytes>,
        pool: Pool,
        total_supply: U256,
        (meta_coin, lp_coin): (usize, usize),
    ) -> Self {
        let decimals = vec![6; tokens.len()];
        Self {
            address: Bytes::from(vec![0xba; 20]),
            tokens,
            decimals,
            pool,
            total_supply,
            meta_coin,
            lp_coin,
        }
    }
}

fn read_state<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    address: &Bytes,
    decimals: &[u8],
) -> Result<(Pool, U256), SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let address = AlloyAddress::from_slice(address.as_ref());
    let raw = vm::read_raw_pool_state(
        engine,
        &address,
        CurveVariant::StableSwapNG,
        decimals,
        &Default::default(),
    )?;
    let pool = build_pool(&raw)
        .map_err(|e| SimulationError::FatalError(format!("curve base pool build failed: {e}")))?;
    let total_supply = vm::read_total_supply(engine, &address)?;
    Ok((pool, total_supply))
}
