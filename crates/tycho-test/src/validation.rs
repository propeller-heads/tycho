use std::{collections::HashMap, time::Duration};

use alloy::{
    hex,
    primitives::{Address, Bytes as AlloyBytes},
    rpc::{
        client::ClientBuilder,
        types::{BlockId, TransactionRequest},
    },
    sol,
    sol_types::SolCall,
};
use async_trait::async_trait;
use tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

type ComponentId = Bytes;
type CallTarget = Address;
type CallData = AlloyBytes;

/// Result of a contract call - either successful data or revert information
#[derive(Debug)]
pub enum CallResult {
    Success(AlloyBytes),
    Revert(String),
}

/// Helper function to get a Validator reference from a ProtocolSim trait object
///
/// This centralizes the downcasting logic for protocols that implement the Validator trait.
/// Add new protocol types here as they implement Validator.
///
/// # Arguments
///
/// * `protocol_system` - The protocol system name (e.g., "uniswap_v2")
/// * `state` - The protocol state as a ProtocolSim trait object
///
/// # Returns
///
/// Returns `Some(&dyn Validator)` if the protocol implements Validator, `None` otherwise.
pub fn get_validator<'a>(
    protocol_system: &str,
    state: &'a dyn ProtocolSim,
) -> Option<&'a dyn Validator> {
    match protocol_system {
        "uniswap_v2" => state
            .as_any()
            .downcast_ref::<UniswapV2State>()
            .map(|s| s as &dyn Validator),
        // Add more protocols here as they implement Validator
        _ => None,
    }
}

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Execute batched contract calls using JSON-RPC batch requests
///
/// This function takes a list of (component_id, calls) pairs and executes
/// all the contract calls in a single batch RPC request for efficiency.
/// Returns a map from component_id to the results for that component.
///
/// # Arguments
///
/// * `rpc_url` - The HTTP RPC endpoint URL
/// * `component_calls` - List of (component_id, calls) pairs
/// * `block_id` - The block number to query at
async fn execute_batched_calls(
    rpc_url: &str,
    component_calls: &[(ComponentId, Vec<(CallTarget, CallData)>)],
    block_id: BlockId,
) -> Result<HashMap<ComponentId, Vec<CallResult>>, Box<dyn std::error::Error + Send + Sync>> {
    if component_calls.is_empty() {
        return Ok(HashMap::new());
    }
    let client = ClientBuilder::default().http(rpc_url.parse()?);
    let mut batch = client.new_batch();

    // Track which futures belong to which component
    let mut component_futures: Vec<(Bytes, Vec<_>)> = Vec::new();

    for (component_id, calls) in component_calls {
        let mut futures = Vec::new();
        for (contract_address, call_data) in calls {
            let tx = TransactionRequest::default()
                .to(*contract_address)
                .input(call_data.clone().into());
            let fut = batch.add_call("eth_call", &(tx, block_id))?;
            futures.push(fut);
        }

        component_futures.push((component_id.clone(), futures));
    }
    batch.send().await?;

    let mut results = HashMap::new();
    for (component_id, futures) in component_futures {
        let mut component_results = Vec::new();
        for fut in futures {
            match fut.await {
                Ok(bytes) => {
                    component_results.push(CallResult::Success(bytes));
                }
                Err(err) => {
                    let error_msg = format!("Call reverted: {}", err);
                    component_results.push(CallResult::Revert(error_msg));
                }
            }
        }
        results.insert(component_id, component_results);
    }

    Ok(results)
}

/// Trait for validating protocol states against on-chain data
///
/// This trait uses a two-phase approach to enable batching of RPC calls:
/// 1. `prepare_validation_calls`: Determines what RPC calls to make for a component
/// 2. `validate_with_results`: Compares the results against the state
#[async_trait]
pub trait Validator: ProtocolSim {
    /// Prepare validation calls for a component
    ///
    /// Returns a list of (contract_address, call_data) pairs representing the contract
    /// calls that need to be made to validate the given component. The call data should
    /// be ABI-encoded function calls (e.g., getReserves, etc.).
    ///
    /// # Arguments
    ///
    /// * `component_id` - The component/pool address
    ///
    /// # Returns
    ///
    /// Returns a vector of (contract_address, call_data) tuples representing
    /// the contract calls that need to be made, or an error if preparation fails.
    fn prepare_validation_calls(
        &self,
        component_id: &ComponentId,
    ) -> Result<Vec<(CallTarget, CallData)>, Box<dyn std::error::Error + Send + Sync>>;

    /// Validate with batched results
    ///
    /// Takes the results from batched RPC calls and validates them against the state.
    /// The results must be in the same order as returned by `prepare_validation_calls`.
    ///
    /// # Arguments
    ///
    /// * `component_id` - The component/pool address
    /// * `results` - The results from the batched RPC calls (successful or reverted)
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if validation passes, `Ok(false)` if validation fails,
    /// or `Err` if there was an error during validation.
    fn validate_with_results(
        &self,
        component_id: &ComponentId,
        results: &[CallResult],
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

/// Maximum number of times the whole validation batch is retried when the RPC reports the queried
/// block is not yet available. The node can lag behind Tycho (especially on fast chains like Base),
/// and load-balanced endpoints may route a retry to a backend that has already synced the block.
/// Matches the swap-simulation retry budget (~100s worst case).
const BLOCK_NOT_FOUND_MAX_RETRIES: u32 = 20;

/// Batch validation for multiple components across any protocol types.
///
/// Validates multiple components in a single batched RPC request. When the RPC reports the queried
/// block is not yet available (the node lags behind Tycho), the whole batch is retried with
/// exponential backoff up to [`BLOCK_NOT_FOUND_MAX_RETRIES`] times rather than surfacing spurious
/// validation failures.
///
/// # Arguments
///
/// * `rpc_url` - The HTTP RPC endpoint URL
/// * `components` - List of (state, component_id) tuples to validate
/// * `block_id` - The block number to query at
///
/// # Returns
///
/// Returns a vector of validation results in the same order as the input components.
/// Each result is `Ok(true)` if validation passed, `Ok(false)` if it failed,
/// or `Err` if there was an error.
pub async fn batch_validate_components(
    rpc_url: &str,
    components: &[(&dyn Validator, ComponentId)],
    block_id: BlockId,
) -> Vec<Result<bool, Box<dyn std::error::Error + Send + Sync>>> {
    let mut backoff = Duration::from_millis(1000);
    let mut attempt = 0;
    loop {
        let results = validate_batch_once(rpc_url, components, block_id).await;

        // All calls in the batch share one block_id against one backend, so a lagging node fails
        // them all with "block not found". Retry the whole batch — a fresh client each attempt may
        // hit an already-synced backend — rather than recording spurious validation failures.
        let block_lagging = results
            .iter()
            .any(|result| match result {
                Err(e) => crate::is_block_not_found(&e.to_string()),
                Ok(_) => false,
            });

        if !block_lagging || attempt >= BLOCK_NOT_FOUND_MAX_RETRIES {
            return results;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
        attempt += 1;
    }
}

async fn validate_batch_once(
    rpc_url: &str,
    components: &[(&dyn Validator, ComponentId)],
    block_id: BlockId,
) -> Vec<Result<bool, Box<dyn std::error::Error + Send + Sync>>> {
    if components.is_empty() {
        return Vec::new();
    }

    // Prepare calls for each component individually
    let mut component_calls = Vec::with_capacity(components.len());

    for (state, component_id) in components {
        match state.prepare_validation_calls(component_id) {
            Ok(calls) => {
                component_calls.push((component_id.clone(), calls));
            }
            Err(e) => {
                return components
                    .iter()
                    .map(|_| Err(format!("Failed to prepare calls: {}", e).into()))
                    .collect();
            }
        }
    }

    // Execute all calls in a single batch, getting results organized by component_id
    let results_map = match execute_batched_calls(rpc_url, &component_calls, block_id).await {
        Ok(results) => results,
        Err(e) => {
            return components
                .iter()
                .map(|_| Err(format!("Batch RPC call failed: {}", e).into()))
                .collect();
        }
    };

    // Validate each component with its results
    let mut results = Vec::with_capacity(components.len());

    for (state, component_id) in components {
        match results_map.get(component_id) {
            Some(component_results) => {
                let validation_result =
                    state.validate_with_results(component_id, component_results);
                results.push(validation_result);
            }
            None => {
                results.push(Err(format!(
                    "No results found for component {}",
                    hex::encode(component_id)
                )
                .into()));
            }
        }
    }

    results
}

#[async_trait]
impl Validator for UniswapV2State {
    fn prepare_validation_calls(
        &self,
        component_id: &ComponentId,
    ) -> Result<Vec<(CallTarget, CallData)>, Box<dyn std::error::Error + Send + Sync>> {
        let pool_address = CallTarget::from_slice(&component_id[..20]);

        // Encode getReserves call for the pool
        let call = IUniswapV2Pair::getReservesCall {}.abi_encode();
        let calls = vec![(pool_address, call.into())];
        Ok(calls)
    }

    fn validate_with_results(
        &self,
        component_id: &ComponentId,
        results: &[CallResult],
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if results.len() != 1 {
            return Err(format!(
                "Expected 1 result for UniswapV2 validation, got {}",
                results.len()
            )
            .into());
        }

        // Check if the call was successful
        let result_data = match &results[0] {
            CallResult::Success(data) => data,
            CallResult::Revert(error) => {
                return Err(format!("getReserves call reverted: {}", error).into());
            }
        };

        // Decode getReserves return value
        let reserves = IUniswapV2Pair::getReservesCall::abi_decode_returns(result_data)?;

        let onchain_reserve0 = alloy::primitives::U256::from(reserves.reserve0);
        let onchain_reserve1 = alloy::primitives::U256::from(reserves.reserve1);

        let reserves_match = self.reserve0 == onchain_reserve0 && self.reserve1 == onchain_reserve1;

        if !reserves_match {
            tracing::warn!(
                component_id = %hex::encode(component_id),
                state_reserve0 = %self.reserve0,
                state_reserve1 = %self.reserve1,
                onchain_reserve0 = %onchain_reserve0,
                onchain_reserve1 = %onchain_reserve1,
                "UniswapV2 reserve validation failed: state reserves do not match on-chain reserves"
            );
        }

        Ok(reserves_match)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;

    use super::*;

    #[tokio::test]
    #[ignore] // This test requires an RPC connection
    async fn test_batch_validate_multiple_components() {
        let block_id = BlockId::from(23775987);

        // Component with correct reserves
        let pool_id_1 = "0x132BC4EA9E5282889fDcfE7Bc7A91Ea901a686D6";

        let state_1 = UniswapV2State::new(
            U256::from(7791135770602459893220844917132_u128),
            U256::from(80274590426947493401_u128),
        );
        let component_id_1 = Bytes::from_str(pool_id_1).unwrap();

        // Component with incorrect reserves to show validation failure
        let state_2 = UniswapV2State::new(U256::from(1000), U256::from(2000));
        let component_id_2 = Bytes::from_str(pool_id_1).unwrap(); // Same component but wrong state

        let rpc_url = std::env::var("RPC_URL")
            .expect("RPC_URL environment variable must be set for this test");

        // Batch validate both components
        let components = vec![
            (&state_1 as &dyn Validator, component_id_1.clone()),
            (&state_2 as &dyn Validator, component_id_2.clone()),
        ];

        let results = batch_validate_components(&rpc_url, &components, block_id).await;

        assert_eq!(results.len(), 2, "Should get results for both components");

        // First component should pass (correct reserves)
        assert!(results[0].as_ref().unwrap(), "Component 1 validation should pass");

        // Second component should fail (incorrect reserves)
        assert!(!results[1].as_ref().unwrap(), "Component 2 validation should fail");
    }
}
