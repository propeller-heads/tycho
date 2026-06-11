use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{map::AddressHashMap, Address, U256},
    rpc::types::{state::AccountOverride, Block},
    sol_types::SolValue,
};
use miette::miette;
use tokio_retry2::{
    strategy::{jitter, ExponentialFactorBackoff},
    Retry, RetryError,
};
use tycho_execution::encoding::evm::utils::bytes_to_address;
use tycho_simulation::evm::protocol::u256_num::u256_to_biguint;

use crate::{
    execution::{
        encoding::{detect_token_slots, setup_router_overwrites},
        execution_simulator::ExecutionSimulator,
        models::{
            RouterOverwritesData, SimulationInput, SimulationResult, TychoExecutionInput,
            TychoExecutionResult,
        },
    },
    RPCTools,
};

pub mod encoding;
mod execution_simulator;
mod four_byte_client;
pub mod models;
pub mod tenderly;
mod traces;

pub async fn simulate_swap_transaction(
    rpc_tools: &RPCTools,
    execution_info: HashMap<String, TychoExecutionInput>,
    block: &Block,
    router_overwrites_data: Option<RouterOverwritesData>,
) -> Result<
    HashMap<String, TychoExecutionResult>,
    (miette::Error, Option<AddressHashMap<AccountOverride>>, Option<tenderly::OverwriteMetadata>),
> {
    let mut inputs: HashMap<String, SimulationInput> = HashMap::new();
    let mut tycho_execution_results: HashMap<String, TychoExecutionResult> = HashMap::new();

    // Get to_address from the first transaction (same for all transactions - tycho router)
    let to_address = execution_info
        .values()
        .next()
        .map(|info| info.transaction.to().clone())
        .expect("To address must be set");

    // Gather all unique token addresses for batch slot detection
    let token_addresses: Vec<_> = execution_info
        .values()
        .map(|info| info.solution.token_in().clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let token_slots = detect_token_slots(rpc_tools, block, &token_addresses, &to_address).await;

    let router_overwrites: Option<AddressHashMap<AccountOverride>> =
        if let Some(router_overwrites_data) = router_overwrites_data {
            Some(
                setup_router_overwrites(
                    bytes_to_address(&to_address).map_err(|e| (miette!("{e}"), None, None))?,
                    router_overwrites_data.router_bytecode,
                    router_overwrites_data.executor_bytecode,
                    router_overwrites_data.fee_calculator_bytecode,
                )
                .await
                .map_err(|e| (e, None, None))?,
            )
        } else {
            None
        };

    for (simulation_id, info) in &execution_info {
        let request = match encoding::swap_request(&info.transaction, block) {
            Ok(request) => request,
            Err(e) => {
                tycho_execution_results.insert(
                    simulation_id.clone(),
                    TychoExecutionResult::Failed {
                        error_msg: format!("Failed to create swap request: {}", e),
                    },
                );
                continue;
            }
        };

        let (mut state_overwrites, metadata) = match token_slots.get(info.solution.token_in()) {
            Some(slots) => encoding::setup_user_overwrites(
                &to_address,
                info.solution.token_in(),
                info.solution.amount_in(),
                slots,
            ),
            None => {
                tycho_execution_results.insert(
                    simulation_id.clone(),
                    TychoExecutionResult::Failed {
                        error_msg: format!(
                            "Couldn't find storage slots for token {}",
                            info.solution.token_in()
                        ),
                    },
                );
                continue;
            }
        };
        if let Some(ref router_overwrites) = router_overwrites {
            state_overwrites.extend(router_overwrites.clone());
        }

        // Add protocol-specific overwrites for Angstrom hooks
        if let Some(first_swap) = info.solution.swaps().first() {
            if let Some(hook_identifier) = first_swap
                .component()
                .static_attributes
                .get("hook_identifier")
            {
                if let Ok(hook_id_str) = std::str::from_utf8(hook_identifier) {
                    if hook_id_str == "angstrom_v1" {
                        let angstrom_address =
                            Address::from_str("0x0000000aa232009084Bd71A5797d089AA4Edfad4")
                                .map_err(|e| {
                                    (miette!("Invalid Angstrom address: {e}"), None, None)
                                })?;
                        let angstrom_overwrites = encoding::setup_angstrom_overwrites(
                            angstrom_address,
                            block.header.number,
                        );
                        state_overwrites.extend(angstrom_overwrites);
                    }
                }
            }
        }

        inputs.insert(
            simulation_id.clone(),
            SimulationInput {
                tx: request,
                state_overwrites: Some(state_overwrites),
                overwrite_metadata: Some(metadata),
            },
        );
    }

    // If no transactions left to simulate, return early
    if inputs.is_empty() {
        return Ok(tycho_execution_results);
    }

    // Use debug_traceCall from the start with retry logic
    // Worst case, it will take ~100s (20 retries with max 5s delay)
    let retry_strategy = ExponentialFactorBackoff::from_millis(1000, 2.)
        .max_delay_millis(5000)
        .map(jitter)
        .take(20);

    // Clone inputs before moving into closure
    let inputs_for_retry = inputs.clone();
    let execution_results = Retry::spawn(retry_strategy, move || {
        let mut simulator = ExecutionSimulator::new(rpc_tools.rpc_url.clone());
        let inputs = inputs_for_retry.clone();

        async move {
            match simulator
                .batch_simulate_with_trace(inputs, block)
                .await
            {
                Ok(res) => Ok(res),
                Err(e) => Err(RetryError::transient(e)),
            }
        }
    })
    .await
    .map_err(|e| {
        (miette!("{e}").wrap_err("Failed to simulate transaction after retries"), None, None)
    })?;

    // Process simulation results and add successful simulations to tycho_execution_results
    for (simulation_id, result) in execution_results {
        match result {
            SimulationResult::Success { return_data, gas_used } => {
                match U256::abi_decode(&return_data) {
                    Ok(amount_out) => {
                        let simulation_input = inputs
                            .get(&simulation_id)
                            .expect("Simulation must be present in inputs HashMap")
                            .clone();
                        let overwrite_metadata = simulation_input.overwrite_metadata;
                        let state_overwrites = simulation_input.state_overwrites;
                        tycho_execution_results.insert(
                            simulation_id,
                            TychoExecutionResult::Success {
                                amount_out: u256_to_biguint(amount_out),
                                gas_used,
                                state_overwrites,
                                overwrite_metadata,
                            },
                        );
                    }
                    Err(e) => {
                        tycho_execution_results.insert(
                            simulation_id,
                            TychoExecutionResult::Failed {
                                error_msg: format!("Failed to decode swap amount: {e:?}"),
                            },
                        );
                    }
                }
            }
            SimulationResult::Revert { reason } => {
                let simulation_input = inputs
                    .get(&simulation_id)
                    .expect("Simulation must be present in inputs HashMap")
                    .clone();
                let overwrite_metadata = simulation_input.overwrite_metadata;
                let state_overwrites = simulation_input.state_overwrites;
                tycho_execution_results.insert(
                    simulation_id,
                    TychoExecutionResult::Revert { reason, state_overwrites, overwrite_metadata },
                );
            }
        }
    }

    Ok(tycho_execution_results)
}
