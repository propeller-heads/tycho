use std::fmt::Debug;

use alloy::{
    core::sol,
    primitives::{Address as AlloyAddress, U256},
    sol_types::SolCall,
};
use revm::DatabaseRef;
use tycho_common::{models::Address, simulation::errors::SimulationError, Bytes};
use tycho_ethereum::BytesCodec;

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    simulation::{SimulationEngine, SimulationEngineError, SimulationParameters},
};

sol! {
    function totalSupply() external view returns (uint256);
    function totalAssets() external view returns (uint256);
    function totalYieldToken() external view returns (uint256);
    function previewRedeem(uint256 shares) external view returns (uint256, uint256);
    function getStableTokenPriceWithScale() external view returns (uint256);
    function stableDepegPrice() external view returns (uint256);
    function stableToken() external view returns (address);
    function yieldToken() external view returns (address);
}

/// The fxSP and fxSAVE state a deposit is priced from, read at the engine's block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultReadings {
    /// fxSP's `stableToken` (USDC).
    pub stable_token: Bytes,
    /// fxSP's `yieldToken` (fxUSD).
    pub yield_token: Bytes,
    /// fxUSD held by fxSP (`totalYieldToken`).
    pub total_yield_token: U256,
    /// USDC held by fxSP, idle and in its strategy, as its next deposit syncs it.
    pub total_stable_token: U256,
    /// fxSP share supply.
    pub base_pool_supply: U256,
    /// USDC price scaled to 18 decimals of USDC, or `None` when the oracle reverts (Chainlink
    /// older than its heartbeat), which reverts every deposit.
    pub stable_price: Option<U256>,
    /// fxSP's `stableDepegPrice`: deposits revert while the USDC price is below it.
    pub stable_depeg_price: U256,
    /// fxSP held by fxSAVE, idle and staked (`totalAssets`).
    pub vault_assets: U256,
    /// fxSAVE share supply.
    pub vault_supply: U256,
}

/// Reads the state fxSP and fxSAVE price a deposit from.
pub fn read_vault<D>(
    vm_engine: &SimulationEngine<D>,
    vault: &Address,
    base_pool: &Address,
) -> Result<VaultReadings, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let base_pool_supply = call(vm_engine, base_pool, &totalSupplyCall {}, "fxSP totalSupply")?;
    // `previewRedeem` divides by the supply; an empty pool holds nothing.
    let total_stable_token = if base_pool_supply.is_zero() {
        U256::ZERO
    } else {
        call(
            vm_engine,
            base_pool,
            &previewRedeemCall { shares: base_pool_supply },
            "fxSP previewRedeem",
        )?
        ._1
    };
    Ok(VaultReadings {
        stable_token: call(vm_engine, base_pool, &stableTokenCall {}, "fxSP stableToken")?
            .to_bytes(),
        yield_token: call(vm_engine, base_pool, &yieldTokenCall {}, "fxSP yieldToken")?.to_bytes(),
        total_yield_token: call(
            vm_engine,
            base_pool,
            &totalYieldTokenCall {},
            "fxSP totalYieldToken",
        )?,
        total_stable_token,
        base_pool_supply,
        stable_price: read_stable_price(vm_engine, base_pool)?,
        stable_depeg_price: call(
            vm_engine,
            base_pool,
            &stableDepegPriceCall {},
            "fxSP stableDepegPrice",
        )?,
        vault_assets: call(vm_engine, vault, &totalAssetsCall {}, "fxSAVE totalAssets")?,
        vault_supply: call(vm_engine, vault, &totalSupplyCall {}, "fxSAVE totalSupply")?,
    })
}

/// Returns the USDC price, or `None` when the price oracle reverts.
fn read_stable_price<D>(
    vm_engine: &SimulationEngine<D>,
    base_pool: &Address,
) -> Result<Option<U256>, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    match simulate(vm_engine, base_pool, &getStableTokenPriceWithScaleCall {}) {
        Ok(output) => getStableTokenPriceWithScaleCall::abi_decode_returns(&output)
            .map(Some)
            .map_err(|e| {
                SimulationError::FatalError(format!(
                    "fxSP {base_pool}: getStableTokenPriceWithScale decode failed: {e}"
                ))
            }),
        Err(SimulationEngineError::TransactionError { .. }) => Ok(None),
        Err(e) => Err(SimulationError::FatalError(format!(
            "fxSP {base_pool}: getStableTokenPriceWithScale simulate failed: {e}"
        ))),
    }
}

fn call<D, Call>(
    vm_engine: &SimulationEngine<D>,
    target: &Address,
    call: &Call,
    method: &str,
) -> Result<Call::Return, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    Call: SolCall,
{
    let output = simulate(vm_engine, target, call).map_err(|e| {
        SimulationError::FatalError(format!("{method} on {target}: simulate failed: {e}"))
    })?;
    Call::abi_decode_returns(&output).map_err(|e| {
        SimulationError::FatalError(format!("{method} on {target}: decode failed: {e}"))
    })
}

fn simulate<D, Call>(
    vm_engine: &SimulationEngine<D>,
    target: &Address,
    call: &Call,
) -> Result<Vec<u8>, SimulationEngineError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    Call: SolCall,
{
    let params = SimulationParameters {
        caller: AlloyAddress::ZERO,
        to: AlloyAddress::from_bytes(target),
        data: call.abi_encode(),
        ..Default::default()
    };
    vm_engine
        .simulate(&params)
        .map(|result| result.result.to_vec())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_client::feed::BlockHeader;

    use super::*;
    use crate::evm::{
        engine_db::{
            simulation_db::SimulationDB,
            utils::{get_client, get_runtime},
        },
        protocol::fxsave::state::tests::{readings, FXSAVE, FX_BASE_POOL},
    };

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn reads_the_vault_at_a_block() {
        let block = BlockHeader {
            number: 26_353_817,
            hash: Bytes::from_str(
                "0xe67427c7000d368d0511777c21ff502578d76de5d7b8732612fc2486012c8cca",
            )
            .unwrap(),
            timestamp: 1_793_944_103,
            ..Default::default()
        };
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.set_block(Some(block));
        let engine = SimulationEngine::new(db, false);

        let vault_readings = read_vault(
            &engine,
            &Bytes::from_str(FXSAVE).unwrap(),
            &Bytes::from_str(FX_BASE_POOL).unwrap(),
        )
        .unwrap();

        assert_eq!(vault_readings, readings());
    }
}
