use std::fmt::Debug;

use alloy::{
    core::sol,
    primitives::{hex, uint, Address as AlloyAddress, B256, U256},
    sol_types::{Revert, SolCall, SolError},
};
use revm::DatabaseRef;
use serde::{Deserialize, Serialize};
use tracing::warn;
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
    function balanceOf(address account) external view returns (uint256);
    function getThreshold() external view returns (uint256);
}

/// EIP-1967 implementation slot, `keccak256("eip1967.proxy.implementation") - 1`. fxSP and fxSAVE
/// read it on every call they delegate, so DCI indexes it with the rest of their storage.
const IMPLEMENTATION_SLOT: U256 =
    uint!(0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc_U256);

/// The reasons `FxUSDPriceOracle` reverts with when Chainlink's USDC answer is negative or older
/// than the oracle's heartbeat.
const ORACLE_REVERT_REASONS: [&str; 2] = ["invalid", "expired"];

/// The fxSP and fxSAVE state a deposit is priced from, read at the engine's block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultReadings {
    /// The implementation fxSP's proxy delegates to.
    pub base_pool_implementation: AlloyAddress,
    /// The implementation fxSAVE's proxy delegates to.
    pub vault_implementation: AlloyAddress,
    /// fxUSD held by fxSP (`totalYieldToken`).
    pub total_yield_token: U256,
    /// USDC held by fxSP, idle and in its strategy, as its next deposit syncs it.
    pub total_stable_token: U256,
    /// fxSP share supply.
    pub base_pool_supply: U256,
    /// USDC price scaled to 18 decimals of USDC, or `None` when reading it reverts, which every
    /// deposit then does too.
    pub stable_price: Option<U256>,
    /// fxSP's `stableDepegPrice`: deposits revert while the USDC price is below it.
    pub stable_depeg_price: U256,
    /// fxSP held by fxSAVE, idle and staked (`totalAssets`).
    pub vault_assets: U256,
    /// fxSAVE share supply.
    pub vault_supply: U256,
    /// fxSP held by fxSAVE and not yet staked in its gauge.
    pub vault_idle_assets: U256,
    /// fxSAVE's `getThreshold`: a deposit that leaves this much idle fxSP stakes all of it.
    pub stake_threshold: U256,
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
    let vault = alloy_address(vault, "fxSAVE")?;
    let base_pool = alloy_address(base_pool, "fxSP")?;
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
        base_pool_implementation: read_implementation(vm_engine, base_pool)?,
        vault_implementation: read_implementation(vm_engine, vault)?,
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
        vault_idle_assets: call(
            vm_engine,
            base_pool,
            &balanceOfCall { account: vault },
            "fxSP balanceOf(fxSAVE)",
        )?,
        stake_threshold: call(vm_engine, vault, &getThresholdCall {}, "fxSAVE getThreshold")?,
    })
}

/// Reads fxSP's stable token (USDC) and yield token (fxUSD), in that order.
pub fn read_tokens<D>(
    vm_engine: &SimulationEngine<D>,
    base_pool: &Address,
) -> Result<(Bytes, Bytes), SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let base_pool = alloy_address(base_pool, "fxSP")?;
    let stable_token = call(vm_engine, base_pool, &stableTokenCall {}, "fxSP stableToken")?;
    let yield_token = call(vm_engine, base_pool, &yieldTokenCall {}, "fxSP yieldToken")?;
    Ok((stable_token.to_bytes(), yield_token.to_bytes()))
}

fn alloy_address(address: &Address, contract: &str) -> Result<AlloyAddress, SimulationError> {
    AlloyAddress::try_from(address.as_ref()).map_err(|e| {
        SimulationError::FatalError(format!(
            "{contract} address {address} is not 20 bytes long: {e}"
        ))
    })
}

fn read_implementation<D>(
    vm_engine: &SimulationEngine<D>,
    proxy: AlloyAddress,
) -> Result<AlloyAddress, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let word = vm_engine
        .state
        .storage_ref(proxy, IMPLEMENTATION_SLOT)
        .map_err(|e| {
            SimulationError::FatalError(format!(
                "Proxy {proxy}: reading its EIP-1967 implementation slot failed: {e:?}"
            ))
        })?;
    Ok(AlloyAddress::from_word(B256::from(word)))
}

/// Returns the USDC price, or `None` when the price call reverts or halts.
///
/// The oracle's own reverts (a negative or stale Chainlink answer) close deposits as expected.
/// Any other failure closes them too, since a deposit would make the same call, but is logged:
/// it points at state DCI did not index or at an upgraded contract, not at the price feed.
fn read_stable_price<D>(
    vm_engine: &SimulationEngine<D>,
    base_pool: AlloyAddress,
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
        Err(SimulationEngineError::TransactionError { data, .. }) => {
            if !is_oracle_revert(&data) {
                warn!(
                    %base_pool,
                    failure = %revert_reason(&data).unwrap_or(data),
                    "fxSP getStableTokenPriceWithScale failed for a reason other than the USDC \
                     oracle's own checks; deposits are closed. Check the oracle's and Chainlink's \
                     storage is indexed and whether either contract was upgraded."
                );
            }
            Ok(None)
        }
        Err(e) => Err(SimulationError::FatalError(format!(
            "fxSP {base_pool}: getStableTokenPriceWithScale simulate failed: {e}"
        ))),
    }
}

/// Whether a failed call's data is one of `FxUSDPriceOracle`'s own reverts.
fn is_oracle_revert(data: &str) -> bool {
    revert_reason(data).is_some_and(|reason| ORACLE_REVERT_REASONS.contains(&reason.as_str()))
}

/// The message of a `revert("...")`, when the failure is one.
fn revert_reason(data: &str) -> Option<String> {
    let output = hex::decode(data.strip_prefix("0x")?).ok()?;
    Revert::abi_decode(&output)
        .ok()
        .map(|revert| revert.reason)
}

fn call<D, Call>(
    vm_engine: &SimulationEngine<D>,
    target: AlloyAddress,
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
    target: AlloyAddress,
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
        to: target,
        data: call.abi_encode(),
        ..Default::default()
    };
    vm_engine
        .simulate(&params)
        .map(|result| result.result.to_vec())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use alloy::primitives::{address, keccak256};
    use revm::state::{AccountInfo, Bytecode};
    use rstest::rstest;
    use tycho_client::feed::BlockHeader;

    use super::*;
    use crate::evm::{
        engine_db::{
            simulation_db::SimulationDB,
            tycho_db::PreCachedDB,
            utils::{get_client, get_runtime},
        },
        protocol::fxsave::state::tests::{readings, FXSAVE, FXUSD, FX_BASE_POOL, USDC},
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
        let vault = Bytes::from_str(FXSAVE).unwrap();
        let base_pool = Bytes::from_str(FX_BASE_POOL).unwrap();

        assert_eq!(read_vault(&engine, &vault, &base_pool).unwrap(), readings());
        assert_eq!(
            read_tokens(&engine, &base_pool).unwrap(),
            (Bytes::from_str(USDC).unwrap(), Bytes::from_str(FXUSD).unwrap())
        );
    }

    const BASE_POOL: AlloyAddress = address!("65c9a641afceb9c0e6034e558a319488fa0fa3be");

    /// An engine holding fxSP as `code` and `storage`.
    fn engine_with(code: Vec<u8>, storage: HashMap<U256, U256>) -> SimulationEngine<PreCachedDB> {
        let db = PreCachedDB::new().unwrap();
        let bytecode = Bytecode::new_raw(code.into());
        db.init_account(
            BASE_POOL,
            AccountInfo::new(U256::ZERO, 0, bytecode.hash_slow(), bytecode),
            Some(storage),
            true,
        )
        .unwrap();
        db.init_account(AlloyAddress::ZERO, AccountInfo::default(), None, true)
            .unwrap();
        db.update(Vec::new(), Some(BlockHeader { number: 1, timestamp: 2, ..Default::default() }))
            .unwrap();
        SimulationEngine::new(db, false)
    }

    /// An engine whose fxSP answers every call by running `code`.
    fn engine_running(code: Vec<u8>) -> SimulationEngine<PreCachedDB> {
        engine_with(code, HashMap::new())
    }

    #[test]
    fn reads_the_implementation_from_the_eip1967_slot() {
        let implementation = address!("9cfefd90d4c8428d4cbac9baaa6d52c6ba7897f9");
        let slot = U256::from_be_bytes(keccak256("eip1967.proxy.implementation").0) - U256::ONE;
        let engine = engine_with(
            vec![0x00],
            HashMap::from([(slot, U256::from_be_slice(implementation.as_slice()))]),
        );

        assert_eq!(read_implementation(&engine, BASE_POOL).unwrap(), implementation);
    }

    /// Runtime code that reverts with `output`: CODECOPY the bytes after the 12-byte prologue to
    /// memory, then REVERT with them.
    fn reverting_with(output: &[u8]) -> Vec<u8> {
        let len = u8::try_from(output.len()).unwrap();
        let mut code = vec![0x60, len, 0x60, 12, 0x60, 0x00, 0x39, 0x60, len, 0x60, 0x00, 0xfd];
        code.extend_from_slice(output);
        code
    }

    fn revert_string(reason: &str) -> Vec<u8> {
        Revert::from(reason).abi_encode()
    }

    #[rstest]
    #[case::stale_answer(reverting_with(&revert_string("expired")))]
    #[case::negative_answer(reverting_with(&revert_string("invalid")))]
    #[case::other_revert_reason(reverting_with(&revert_string("Pausable: paused")))]
    #[case::custom_error(reverting_with(&[0x12, 0x34, 0x56, 0x78]))]
    #[case::halt(vec![0xfe])]
    fn a_failing_price_call_closes_deposits(#[case] code: Vec<u8>) {
        let engine = engine_running(code);

        assert_eq!(read_stable_price(&engine, BASE_POOL).unwrap(), None);
    }

    #[rstest]
    #[case::stale_answer(format!("0x{}", hex::encode(revert_string("expired"))), true)]
    #[case::negative_answer(format!("0x{}", hex::encode(revert_string("invalid"))), true)]
    #[case::other_revert_reason(format!("0x{}", hex::encode(revert_string("paused"))), false)]
    #[case::custom_error("0x12345678".to_string(), false)]
    #[case::halt("InvalidFEOpcode".to_string(), false)]
    fn only_the_oracle_reverts_are_expected(#[case] data: String, #[case] expected: bool) {
        assert_eq!(is_oracle_revert(&data), expected);
    }

    #[test]
    fn a_short_address_is_an_error_not_a_panic() {
        let engine = engine_running(vec![0x00]);
        let short = Bytes::from(vec![0x11; 19]);

        assert!(matches!(
            read_vault(&engine, &short, &Bytes::from(BASE_POOL.to_vec())),
            Err(SimulationError::FatalError(message)) if message.contains("not 20 bytes")
        ));
        assert!(matches!(
            read_tokens(&engine, &short),
            Err(SimulationError::FatalError(message)) if message.contains("not 20 bytes")
        ));
    }
}
