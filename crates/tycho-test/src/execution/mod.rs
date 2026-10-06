use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use alloy::{
    primitives::{
        map::{AddressHashMap, B256HashMap},
        Address, B256, U256,
    },
    rpc::types::{state::AccountOverride, Block},
    sol_types::SolValue,
};
use futures::{stream, StreamExt};
use miette::miette;
use tokio_retry2::{
    strategy::{jitter, ExponentialFactorBackoff},
    Retry, RetryError,
};
use tycho_common::Bytes;
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

/// Every `debug_traceCall` carries its full state overwrites, so one unbounded JSON-RPC batch per
/// block outgrows the request size limit of the RPC (HTTP 413) and loses every simulation.
const MAX_CALLS_PER_BATCH: usize = 30;
/// Keeps the per-block throughput of a single batch without flooding the RPC.
const MAX_CONCURRENT_BATCHES: usize = 4;

/// Simulates every encoded swap in `execution_info` against `block` via `debug_traceCall` and
/// returns one result per simulation id.
///
/// `router_overwrites_data` decides which contracts are simulated as deployed and which are
/// overwritten — see [`RouterOverwritesData`]; its default simulates everything as deployed.
///
/// `oracle_overwrites` are storage slots applied to every simulation, merged per account after
/// the balance, allowance and protocol overwrites.
///
/// # Errors
/// Per-simulation failures are reported as `TychoExecutionResult::Failed`/`Revert`. An `Err`
/// signals that the whole batch could not be simulated, and carries the state overwrites and
/// metadata of the batch when they were already built, for Tenderly link generation.
pub async fn simulate_swap_transaction(
    rpc_tools: &RPCTools,
    execution_info: HashMap<String, TychoExecutionInput>,
    block: &Block,
    router_overwrites_data: RouterOverwritesData,
    oracle_overwrites: Option<AddressHashMap<B256HashMap<B256>>>,
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

    let token_slots = detect_token_slots(rpc_tools, &token_addresses, &to_address).await;

    let router_overwrites = setup_router_overwrites(
        bytes_to_address(&to_address).map_err(|e| (miette!("{e}"), None, None))?,
        router_overwrites_data,
    )
    .map_err(|e| (e, None, None))?;

    let fermiswap_pairs = collect_fermiswap_pairs(&execution_info).map_err(|e| (e, None, None))?;
    let fermiswap_overwrites = if fermiswap_pairs.is_empty() {
        None
    } else {
        Some(
            encoding::setup_fermiswap_overwrites(rpc_tools, block, &fermiswap_pairs)
                .await
                .map_err(|e| (e, None, None))?,
        )
    };

    let bopamm_asset_ids =
        collect_bopamm_asset_ids(&execution_info).map_err(|e| (e, None, None))?;
    let bopamm_overwrites = if bopamm_asset_ids.is_empty() {
        None
    } else {
        Some(
            encoding::setup_bopamm_overwrites(rpc_tools, block, &bopamm_asset_ids)
                .await
                .map_err(|e| (e, None, None))?,
        )
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

        let token_in = info.solution.token_in();
        let sells_native = token_in == &Bytes::zero(20);
        let (mut state_overwrites, metadata) = match token_slots.get(token_in) {
            None if sells_native => (
                encoding::setup_native_user_overwrites(info.solution.amount_in()),
                tenderly::OverwriteMetadata::new(),
            ),
            Some(slots) => encoding::setup_user_overwrites(
                &to_address,
                token_in,
                info.solution.amount_in(),
                slots,
            ),
            None => {
                tycho_execution_results.insert(
                    simulation_id.clone(),
                    TychoExecutionResult::Failed {
                        error_msg: format!("Couldn't find storage slots for token {token_in}"),
                    },
                );
                continue;
            }
        };
        state_overwrites.extend(router_overwrites.clone());
        if let Some(ref fermiswap_overwrites) = fermiswap_overwrites {
            state_overwrites.extend(fermiswap_overwrites.clone());
        }
        if let Some(ref bopamm_overwrites) = bopamm_overwrites {
            state_overwrites.extend(bopamm_overwrites.clone());
        }
        if let Some(ref oracle_overwrites) = oracle_overwrites {
            merge_storage_overwrites(&mut state_overwrites, oracle_overwrites);
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

    let execution_results = simulate_in_batches(&rpc_tools.rpc_url, &inputs, block)
        .await
        .map_err(|e| (e, None, None))?;

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

/// Simulates `inputs` against `block` in JSON-RPC batches of at most [`MAX_CALLS_PER_BATCH`]
/// calls, [`MAX_CONCURRENT_BATCHES`] of them in flight at once, and returns one result per
/// simulation id.
///
/// # Errors
/// Returns an error naming the failed batch when any batch still fails after its retries; the
/// results of the other batches are discarded.
async fn simulate_in_batches(
    rpc_url: &str,
    inputs: &HashMap<String, SimulationInput>,
    block: &Block,
) -> miette::Result<HashMap<String, SimulationResult>> {
    let batches = split_into_batches(inputs, MAX_CALLS_PER_BATCH);
    let n_batches = batches.len();

    let mut pending = stream::iter(batches.into_iter().enumerate())
        .map(|(index, batch)| async move {
            (index, simulate_batch_with_retry(rpc_url, batch, block).await)
        })
        .buffer_unordered(MAX_CONCURRENT_BATCHES);

    let mut results = HashMap::with_capacity(inputs.len());
    while let Some((index, batch_results)) = pending.next().await {
        let batch_results = batch_results.map_err(|e| {
            miette!("{e}").wrap_err(format!(
                "Failed to simulate batch {} of {n_batches} after retries",
                index + 1
            ))
        })?;
        results.extend(batch_results);
    }
    Ok(results)
}

/// Sends one JSON-RPC batch of `debug_traceCall`s, retrying the whole batch on any error.
/// Worst case this takes ~100s (20 retries with at most 5s delay).
async fn simulate_batch_with_retry(
    rpc_url: &str,
    inputs: HashMap<String, SimulationInput>,
    block: &Block,
) -> Result<HashMap<String, SimulationResult>, String> {
    let retry_strategy = ExponentialFactorBackoff::from_millis(1000, 2.)
        .max_delay_millis(5000)
        .map(jitter)
        .take(20);

    Retry::spawn(retry_strategy, || {
        let mut simulator = ExecutionSimulator::new(rpc_url.to_string());
        let inputs = inputs.clone();
        async move {
            simulator
                .batch_simulate_with_trace(inputs, block)
                .await
                .map_err(|e| RetryError::transient(e.to_string()))
        }
    })
    .await
}

fn split_into_batches(
    inputs: &HashMap<String, SimulationInput>,
    batch_size: usize,
) -> Vec<HashMap<String, SimulationInput>> {
    let mut batches = Vec::with_capacity(inputs.len().div_ceil(batch_size));
    let mut batch = HashMap::with_capacity(batch_size);
    for (simulation_id, input) in inputs {
        batch.insert(simulation_id.clone(), input.clone());
        if batch.len() == batch_size {
            batches.push(std::mem::replace(&mut batch, HashMap::with_capacity(batch_size)));
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Adds `storage` to one simulation's overwrites, slot by slot.
fn merge_storage_overwrites(
    overwrites: &mut AddressHashMap<AccountOverride>,
    storage: &AddressHashMap<B256HashMap<B256>>,
) {
    for (address, slots) in storage {
        overwrites
            .entry(*address)
            .or_default()
            .state_diff
            .get_or_insert_with(Default::default)
            .extend(
                slots
                    .iter()
                    .map(|(slot, value)| (*slot, *value)),
            );
    }
}

fn collect_fermiswap_pairs(
    execution_info: &HashMap<String, TychoExecutionInput>,
) -> miette::Result<Vec<(Address, Address)>> {
    let mut pairs = HashSet::new();

    for info in execution_info.values() {
        for swap in info.solution.swaps() {
            let component = swap.component();
            if component.protocol_system != "vm:fermiswap" {
                continue;
            }

            if component.tokens.len() < 2 {
                return Err(miette!(
                    "FermiSwap component {:?} has {} tokens; expected at least 2",
                    component.id,
                    component.tokens.len()
                ));
            }

            let base_asset = bytes_to_address(&component.tokens[0])
                .map_err(|e| miette!("Invalid FermiSwap base asset address: {e}"))?;
            let quote_asset = bytes_to_address(&component.tokens[1])
                .map_err(|e| miette!("Invalid FermiSwap quote asset address: {e}"))?;
            pairs.insert((base_asset, quote_asset));
        }
    }

    Ok(pairs.into_iter().collect())
}

fn collect_bopamm_asset_ids(
    execution_info: &HashMap<String, TychoExecutionInput>,
) -> miette::Result<Vec<U256>> {
    let mut asset_ids = HashSet::new();

    for info in execution_info.values() {
        for swap in info.solution.swaps() {
            let component = swap.component();
            if component.protocol_system != "vm:bopamm" {
                continue;
            }

            let asset_id = component
                .static_attributes
                .get("asset_id")
                .ok_or_else(|| {
                    miette!(
                        "BopAMM component {:?} is missing the asset_id static attribute",
                        component.id
                    )
                })?;
            asset_ids.insert(U256::from_be_slice(asset_id.as_ref()));
        }
    }

    Ok(asset_ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{address, b256},
        rpc::types::TransactionRequest,
    };
    use mockito::{Mock, Request, ServerGuard};
    use serde_json::{json, Value};

    use super::*;

    const REGISTRY: Address = address!("da7afeed01fe625cf15d187a19f94b45f00b8c5f");
    const LANE: B256 = b256!("0000000000000000000000000000000000000000000000000000000000000001");
    const OTHER_LANE: B256 =
        b256!("0000000000000000000000000000000000000000000000000000000000000002");
    const VALUE: B256 = b256!("00000000000000000000000000000000000000000000000000000000000000ff");

    #[test]
    fn an_account_with_another_slot_overwritten() {
        let mut overwrites = AddressHashMap::default();
        overwrites
            .insert(REGISTRY, AccountOverride::default().with_state_diff(vec![(LANE, VALUE)]));
        let storage =
            AddressHashMap::from_iter([(REGISTRY, B256HashMap::from_iter([(OTHER_LANE, VALUE)]))]);

        merge_storage_overwrites(&mut overwrites, &storage);

        let state_diff = overwrites[&REGISTRY]
            .state_diff
            .as_ref()
            .expect("state diff is set");
        assert_eq!(state_diff.get(&LANE), Some(&VALUE));
        assert_eq!(state_diff.get(&OTHER_LANE), Some(&VALUE));
    }

    #[test]
    fn an_account_with_no_overwrites() {
        let mut overwrites = AddressHashMap::default();
        let storage =
            AddressHashMap::from_iter([(REGISTRY, B256HashMap::from_iter([(LANE, VALUE)]))]);

        merge_storage_overwrites(&mut overwrites, &storage);

        assert_eq!(
            overwrites[&REGISTRY]
                .state_diff
                .as_ref()
                .and_then(|slots| slots.get(&LANE)),
            Some(&VALUE)
        );
    }
    fn simulation_inputs(n: usize) -> HashMap<String, SimulationInput> {
        (0..n)
            .map(|i| {
                let input = SimulationInput {
                    tx: TransactionRequest::default(),
                    state_overwrites: None,
                    overwrite_metadata: None,
                };
                (format!("simulation-{i}"), input)
            })
            .collect()
    }

    fn batch_len(request: &Request) -> usize {
        let body: Value = serde_json::from_slice(
            request
                .body()
                .expect("request has a body"),
        )
        .expect("request body is JSON");
        body.as_array().map_or(1, Vec::len)
    }

    /// Answers every call of a JSON-RPC batch with a successful call trace returning `1`.
    fn successful_traces(request: &Request) -> Vec<u8> {
        let body: Value = serde_json::from_slice(
            request
                .body()
                .expect("request has a body"),
        )
        .expect("request body is JSON");
        let calls = body
            .as_array()
            .expect("request is a JSON-RPC batch");
        let responses: Vec<Value> = calls
            .iter()
            .map(|call| {
                json!({
                    "jsonrpc": "2.0",
                    "id": call["id"],
                    "result": {
                        "type": "CALL",
                        "gasUsed": "0x5208",
                        "output": format!("0x{:064x}", 1),
                    },
                })
            })
            .collect();
        serde_json::to_vec(&responses).expect("responses serialize")
    }

    async fn rpc_rejecting_batches_over(
        server: &mut ServerGuard,
        max_calls: usize,
    ) -> (Mock, Mock) {
        let rejected = server
            .mock("POST", "/")
            .match_request(move |request| batch_len(request) > max_calls)
            .with_status(413)
            .create_async()
            .await;
        let answered = server
            .mock("POST", "/")
            .match_request(move |request| batch_len(request) <= max_calls)
            .with_header("content-type", "application/json")
            .with_body_from_request(successful_traces)
            .expect_at_least(1)
            .create_async()
            .await;
        (rejected, answered)
    }

    #[test]
    fn no_batches_for_no_inputs() {
        assert!(split_into_batches(&simulation_inputs(0), 30).is_empty());
    }

    #[test]
    fn batches_cover_every_input_once() {
        let inputs = simulation_inputs(70);

        let batches = split_into_batches(&inputs, 30);

        let mut sizes: Vec<usize> = batches
            .iter()
            .map(HashMap::len)
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![10, 30, 30]);
        let mut ids: Vec<&String> = batches
            .iter()
            .flat_map(HashMap::keys)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), inputs.len());
    }

    #[test]
    fn inputs_filling_whole_batches_leave_no_empty_batch() {
        let batches = split_into_batches(&simulation_inputs(60), 30);

        assert_eq!(
            batches
                .iter()
                .map(HashMap::len)
                .collect::<Vec<_>>(),
            vec![30, 30]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn simulations_beyond_the_rpc_batch_limit_all_get_results() {
        let mut server = mockito::Server::new_async().await;
        let (rejected, answered) = rpc_rejecting_batches_over(&mut server, 30).await;
        let inputs = simulation_inputs(70);

        let results = simulate_in_batches(&server.url(), &inputs, &Block::default())
            .await
            .expect("every batch fits the RPC limit");

        assert_eq!(results.len(), inputs.len());
        for simulation_id in inputs.keys() {
            let Some(SimulationResult::Success { return_data, gas_used }) =
                results.get(simulation_id)
            else {
                panic!(
                    "{simulation_id} has no successful result: {:?}",
                    results.get(simulation_id)
                );
            };
            assert_eq!(U256::abi_decode(return_data).expect("amount out"), U256::ONE);
            assert_eq!(*gas_used, 21_000);
        }
        rejected.expect(0).assert_async().await;
        answered.expect(3).assert_async().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_the_rpc_keeps_rejecting_fails_the_simulation() {
        let mut server = mockito::Server::new_async().await;
        let (_rejected, _answered) = rpc_rejecting_batches_over(&mut server, 0).await;

        let error = simulate_in_batches(&server.url(), &simulation_inputs(5), &Block::default())
            .await
            .expect_err("the RPC rejects every batch");

        assert!(
            format!("{error:?}").contains("Failed to simulate batch 1 of 1 after retries"),
            "unexpected error: {error:?}"
        );
    }
}
