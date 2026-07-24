use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use alloy::{
    primitives::{Address, U256},
    sol_types::SolValue,
};
use revm::DatabaseRef;
use tycho_common::simulation::errors::SimulationError;

use super::{
    erc20_token::Overwrites, models::Capability, tycho_simulation_contract::TychoSimulationContract,
};
use crate::evm::{
    account_storage::StateUpdate,
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::{u256_num::u256_to_f64, vm::utils::string_to_bytes32},
    simulation::BlockEnvOverrides,
};

#[derive(Debug)]
pub struct Trade {
    pub received_amount: U256,
    pub gas_used: U256,
    pub price: f64,
}

/// Type aliases are defined to ensure compatibility with `alloy::sol_types::abi_decode`,
/// which requires explicit types matching the Solidity ABI. These aliases correspond
/// directly to the outputs of the contract's functions.
/// These types ensure correct decoding and alignment with the ABI.
type PriceReturn = Vec<(U256, U256)>;
type SwapReturn = (U256, U256, (U256, U256));
type LimitsReturn = Vec<U256>;
type CapabilitiesReturn = Vec<U256>;
type MinGasUsageReturn = U256;

/// An implementation of `TychoSimulationContract` specific to the `AdapterContract` ABI interface,
/// providing methods for price calculations, token swaps, capability checks, and more.
///
/// This struct facilitates interaction with the `AdapterContract` by encoding and decoding data
/// according to its ABI specification. Each method corresponds to a function in the adapter
/// contract's interface, enabling seamless integration with tycho's simulation environment.
///
/// # Methods
/// - `price`: Calculates price information for a token pair within the adapter.
/// - `swap`: Simulates a token swap operation, returning details about the trade and state updates.
/// - `get_limits`: Retrieves the trade limits for a given token pair.
/// - `get_capabilities`: Checks the capabilities of the adapter for a specific token pair.
/// - `min_gas_usage`: Queries the minimum gas usage required for operations within the adapter.
impl<D: EngineDatabaseInterface + Clone + Debug> TychoSimulationContract<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    pub fn price(
        &self,
        pair_id: &str,
        sell_token: Address,
        buy_token: Address,
        amounts: Vec<U256>,
        overwrites: Option<HashMap<Address, Overwrites>>,
        block_overrides: Option<BlockEnvOverrides>,
    ) -> Result<Vec<f64>, SimulationError> {
        let args = (string_to_bytes32(pair_id)?, sell_token, buy_token, amounts);
        let selector = "price(bytes32,address,address,uint256[])";

        let res = self
            .call(selector, args, overwrites, None, U256::from(0u64), None, block_overrides)?
            .return_value;

        let decoded: PriceReturn = PriceReturn::abi_decode(&res).map_err(|e| {
            SimulationError::FatalError(format!("Failed to decode price return value: {e:?}"))
        })?;

        let price = self.calculate_price(decoded)?;
        Ok(price)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn swap(
        &self,
        pair_id: &str,
        sell_token: Address,
        buy_token: Address,
        is_buy: bool,
        amount: U256,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        block_overrides: Option<BlockEnvOverrides>,
    ) -> Result<(Trade, HashMap<Address, StateUpdate>), SimulationError> {
        let args = (string_to_bytes32(pair_id)?, sell_token, buy_token, is_buy, amount);
        let selector = "swap(bytes32,address,address,uint8,uint256)";

        let res =
            self.call(selector, args, overwrites, None, U256::from(0u64), None, block_overrides)?;

        let decoded: SwapReturn = SwapReturn::abi_decode(&res.return_value).map_err(|_| {
            SimulationError::FatalError(format!(
                "Adapter swap call failed: Failed to decode return value. Expected amount, gas, and price elements in the format (U256, U256, (U256, U256)). Found {:?}",
                        &res.return_value[..],
            ))
        })?;

        let (received_amount, gas_used, price_elements) = decoded;

        let price = self
            .calculate_price(vec![price_elements])?
            .first()
            .cloned()
            .ok_or_else(|| {
                SimulationError::FatalError(
                    "Adapter swap call failed: An empty price list was returned".into(),
                )
            })?;

        Ok((Trade { received_amount, gas_used, price }, res.simulation_result.state_updates))
    }

    pub fn get_limits(
        &self,
        pair_id: &str,
        sell_token: Address,
        buy_token: Address,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        block_overrides: Option<BlockEnvOverrides>,
    ) -> Result<(U256, U256), SimulationError> {
        let args = (string_to_bytes32(pair_id)?, sell_token, buy_token);
        let selector = "getLimits(bytes32,address,address)";

        let res = self
            .call(selector, args, overwrites, None, U256::from(0u64), None, block_overrides)?
            .return_value;

        let decoded: LimitsReturn = LimitsReturn::abi_decode(&res).map_err(|e| {
            SimulationError::FatalError(format!(
                "Adapter get_limits call failed: Failed to decode return value: {e:?}"
            ))
        })?;

        Ok((decoded[0], decoded[1]))
    }

    pub fn get_capabilities(
        &self,
        pair_id: &str,
        sell_token: Address,
        buy_token: Address,
    ) -> Result<HashSet<Capability>, SimulationError> {
        let args = (string_to_bytes32(pair_id)?, sell_token, buy_token);
        let selector = "getCapabilities(bytes32,address,address)";

        let res = self
            .call(selector, args, None, None, U256::from(0u64), None, None)?
            .return_value;
        let decoded: CapabilitiesReturn = CapabilitiesReturn::abi_decode(&res).map_err(|e| {
            SimulationError::FatalError(format!(
                "Adapter get_capabilities call failed: Failed to decode return value: {e:?}"
            ))
        })?;

        let capabilities: HashSet<Capability> = decoded
            .into_iter()
            .filter_map(|value| Capability::from_u256(value).ok())
            .collect();

        Ok(capabilities)
    }

    #[allow(dead_code)]
    pub fn min_gas_usage(&self) -> Result<u64, SimulationError> {
        let args = ();
        let selector = "minGasUsage()";

        let res = self
            .call(selector, args, None, None, U256::from(0u64), None, None)?
            .return_value;

        let decoded: MinGasUsageReturn = MinGasUsageReturn::abi_decode(&res).map_err(|e| {
            SimulationError::FatalError(format!(
                "Adapter min gas usage call failed: Failed to decode return value: {e:?}"
            ))
        })?;
        decoded
            .try_into()
            .map_err(|_| SimulationError::FatalError("Decoded value exceeds u64 range".to_string()))
    }

    fn calculate_price(&self, fractions: Vec<(U256, U256)>) -> Result<Vec<f64>, SimulationError> {
        fractions
            .into_iter()
            .map(|(numerator, denominator)| {
                if denominator.is_zero() {
                    Err(SimulationError::FatalError(
                        "Adapter price calculation failed: Denominator is zero".to_string(),
                    ))
                } else {
                    let num_f64 = u256_to_f64(numerator)?;
                    let den_f64 = u256_to_f64(denominator)?;
                    Ok(num_f64 / den_f64)
                }
            })
            .collect()
    }
}
