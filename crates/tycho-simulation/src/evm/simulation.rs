use std::{clone::Clone, collections::HashMap, default::Default, env, fmt::Debug};

use alloy::primitives::{Address, Bytes, U256};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, Output, ResultAndState},
        BlockEnv, CfgEnv, Context, TxEnv,
    },
    interpreter::{return_ok, InstructionResult},
    primitives::{hardfork::SpecId, TxKind},
    state::EvmState,
    DatabaseRef, ExecuteEvm, InspectEvm, MainBuilder, MainContext,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use strum_macros::Display;
use tokio::runtime::{Handle, Runtime};
use tracing::debug;

use super::{
    account_storage::StateUpdate,
    traces::{handle_traces, TraceResult},
};
use crate::evm::engine_db::{
    engine_db_interface::EngineDatabaseInterface, simulation_db::OverriddenSimulationDB,
};

/// An error representing any transaction simulation result other than successful execution
#[derive(Debug, Display, Clone, PartialEq)]
pub enum SimulationEngineError {
    /// Something went wrong while getting storage; might be caused by network issues.
    /// Retrying may help.
    StorageError(String),
    /// Gas limit has been reached. Retrying while increasing gas limit or waiting for a gas price
    /// reduction may help.
    OutOfGas(String, String),
    /// Simulation didn't succeed; likely not related to network or gas, so retrying won't help
    TransactionError { data: String, gas_used: Option<u64> },
    /// Processing traces failed.
    TraceError(String),
}

/// A result of a successful transaction simulation
#[derive(Debug, Clone, Default)]
pub struct SimulationResult {
    /// Output of transaction execution as bytes
    pub result: Bytes,
    /// State changes caused by the transaction
    pub state_updates: HashMap<Address, StateUpdate>,
    /// Gas used by the transaction (already reduced by the refunded gas)
    pub gas_used: u64,
    /// Transient storage changes captured during the simulation
    pub transient_storage: HashMap<Address, HashMap<U256, U256>>,
}

/// Simulation engine
#[derive(Debug, Clone)]
pub struct SimulationEngine<D: EngineDatabaseInterface + Clone + Debug>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    pub state: D,
    pub trace: bool,
}

impl<D: EngineDatabaseInterface + Clone + Debug> SimulationEngine<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// Create a new simulation engine
    ///
    /// # Arguments
    ///
    /// * `state` - Database reference to be used for simulation
    /// * `trace` - Whether to print the entire execution trace
    ///
    /// # Notes
    /// If you set traces to true, consider setting the ETHERSCAN_API_KEY env variable
    /// so the tracer can pull contract metadata from etherscan.
    pub fn new(state: D, trace: bool) -> Self {
        Self { state, trace }
    }

    /// Simulate a transaction
    ///
    /// State's block will be modified to be the last block before the simulation's block.
    pub fn simulate(
        &self,
        params: &SimulationParameters,
    ) -> Result<SimulationResult, SimulationEngineError> {
        // We allocate a new EVM so we can work with a simple referenced DB instead of a fully
        // concurrently save shared reference and write locked object. Note that concurrently
        // calling this method is therefore not possible.
        // There is no need to keep an EVM on the struct as it only holds the environment and the
        // db, the db is simply a reference wrapper. To avoid lifetimes leaking we don't let the evm
        // struct outlive this scope.

        // We protect the state from being consumed.
        let overrides = params
            .overrides
            .clone()
            .unwrap_or_default();

        let db_ref = OverriddenSimulationDB { inner_db: &self.state, overrides: &overrides };

        let tx_env = TxEnv {
            caller: params.caller,
            gas_limit: params.gas_limit.unwrap_or(8_000_000),
            kind: TxKind::Call(params.to),
            value: params.value,
            data: Bytes::copy_from_slice(&params.data),
            ..Default::default()
        };

        let mut block =
            self.state
                .get_current_block()
                .ok_or(SimulationEngineError::StorageError(
                    "Current block not set in SimulationEngine.".into(),
                ))?;

        if let Some(overrides) = &params.block_overrides {
            if let Some(number) = overrides.number {
                block.number = number;
            }
            if let Some(timestamp) = overrides.timestamp {
                block.timestamp = timestamp;
            }
        }

        let block_env = BlockEnv {
            number: U256::from(block.number),
            timestamp: U256::from(block.timestamp),
            ..Default::default()
        };

        let mut cfg_env: CfgEnv<SpecId> = CfgEnv::new_with_spec(SpecId::PRAGUE);
        cfg_env.disable_nonce_check = true;
        cfg_env.disable_eip3607 = true;

        let context = Context::mainnet()
            .with_cfg(cfg_env)
            .with_ref_db(db_ref)
            .with_block(block_env)
            .with_tx(tx_env.clone())
            .modify_journal_chained(|journal| {
                if let Some(transient_storage) = params.transient_storage.clone() {
                    for (address, slots) in transient_storage {
                        for (slot, value) in slots {
                            journal.tstore(address, slot, value);
                        }
                    }
                }
            });

        let evm_result = if self.trace {
            let mut tracer = TracingInspector::new(TracingInspectorConfig::default());

            let res = {
                let mut vm = context.build_mainnet_with_inspector(&mut tracer);

                debug!(
                    "Starting simulation with tx parameters: {:#?} {:#?}",
                    vm.ctx.tx, vm.ctx.block
                );
                vm.inspect_tx(tx_env.clone())
            };

            Self::print_traces(tracer, res.as_ref().ok())?;

            res
        } else {
            let mut vm = context.build_mainnet();

            debug!("Starting simulation with tx parameters: {:#?} {:#?}", vm.ctx.tx, vm.ctx.block);

            vm.replay()
        };

        // TODO: update revm to 25.0.0 and get transient storage from the journaled state
        interpret_evm_result(evm_result, HashMap::new())
    }

    pub fn clear_temp_storage(&mut self) -> Result<(), <D as EngineDatabaseInterface>::Error> {
        self.state.clear_temp_storage()
    }

    fn print_traces(
        tracer: TracingInspector,
        res: Option<&ResultAndState>,
    ) -> Result<(), SimulationEngineError> {
        let (exit_reason, _gas_refunded, gas_used, _out, _exec_logs) = match res {
            Some(ResultAndState { result, state: _ }) => {
                // let ResultAndState { result, state: _ } = res;
                match result.clone() {
                    ExecutionResult::Success {
                        reason,
                        gas_used,
                        gas_refunded,
                        output,
                        logs,
                        ..
                    } => (reason.into(), gas_refunded, gas_used, Some(output), logs),
                    ExecutionResult::Revert { gas_used, output } => {
                        // Need to fetch the unused gas
                        (
                            InstructionResult::Revert,
                            0_u64,
                            gas_used,
                            Some(Output::Call(output)),
                            vec![],
                        )
                    }
                    ExecutionResult::Halt { reason, gas_used } => {
                        (reason.into(), 0_u64, gas_used, None, vec![])
                    }
                }
            }
            _ => (InstructionResult::Stop, 0_u64, 0, None, vec![]),
        };

        let trace_res = TraceResult {
            success: matches!(exit_reason, return_ok!()),
            traces: Some(vec![tracer.into_traces()]),
            gas_used,
        };

        tokio::task::block_in_place(|| -> Result<(), SimulationEngineError> {
            let future = async {
                handle_traces(
                    trace_res,
                    env::var("ETHERSCAN_API_KEY").ok(),
                    tycho_common::models::Chain::Ethereum,
                )
                .await
                .map_err(|err| SimulationEngineError::TraceError(err.to_string()))
            };
            if let Ok(handle) = Handle::try_current() {
                // If successful, use the existing runtime to block on the future
                handle.block_on(future)
            } else {
                // If no runtime is found, create a new one and block on the future
                let rt = Runtime::new().map_err(|err| {
                    SimulationEngineError::TraceError(format!(
                        "Failed to create a new runtime: {err}"
                    ))
                })?;
                rt.block_on(future)
            }
        })?;

        Ok(())
    }
}

/// Convert a complex EVMResult into a simpler structure
///
/// EVMResult is not of an error type even if the transaction was not successful.
/// This function returns an Ok if and only if the transaction was successful.
/// In case the transaction was reverted, halted, or another error occurred (like an error
/// when accessing storage), this function returns an Err with a simple String description
/// of an underlying cause.
///
/// # Arguments
///
/// * `evm_result` - output from calling `revm.transact()`
///
/// # Errors
///
/// * `SimulationError` - simulation wasn't successful for any reason. See variants for details.
fn interpret_evm_result<DBError: Debug>(
    evm_result: Result<ResultAndState, EVMError<DBError>>,
    transient_storage: HashMap<Address, HashMap<U256, U256>>,
) -> Result<SimulationResult, SimulationEngineError> {
    match evm_result {
        Ok(result_and_state) => match result_and_state.result {
            ExecutionResult::Success { gas_used, gas_refunded, output, .. } => {
                Ok(interpret_evm_success(
                    gas_used,
                    gas_refunded,
                    output,
                    result_and_state.state,
                    transient_storage,
                ))
            }
            ExecutionResult::Revert { output, gas_used } => {
                Err(SimulationEngineError::TransactionError {
                    data: format!("0x{encoded}", encoded = hex::encode::<Vec<u8>>(output.into())),
                    gas_used: Some(gas_used),
                })
            }
            ExecutionResult::Halt { reason, gas_used } => {
                Err(SimulationEngineError::TransactionError {
                    data: format!("{reason:?}"),
                    gas_used: Some(gas_used),
                })
            }
        },
        Err(evm_error) => match evm_error {
            EVMError::Transaction(invalid_tx) => Err(SimulationEngineError::TransactionError {
                data: format!("EVM error: {invalid_tx:?}"),
                gas_used: None,
            }),
            EVMError::Database(db_error) => {
                Err(SimulationEngineError::StorageError(format!("Storage error: {db_error:?}")))
            }
            EVMError::Custom(err) => Err(SimulationEngineError::TransactionError {
                data: format!("Unexpected error {err}"),
                gas_used: None,
            }),
            EVMError::Header(err) => Err(SimulationEngineError::TransactionError {
                data: format!("Unexpected error {err}"),
                gas_used: None,
            }),
        },
    }
}

// Helper function to extract some details from a successful transaction execution
fn interpret_evm_success(
    gas_used: u64,
    gas_refunded: u64,
    output: Output,
    state: EvmState,
    transient_storage: HashMap<Address, HashMap<U256, U256>>,
) -> SimulationResult {
    SimulationResult {
        result: output.into_data(),
        state_updates: {
            // For each account mentioned in state updates in REVM output, we will have
            // one record in our hashmap. Such record contains *new* values of account's
            // state. This record's optional `storage` field will contain
            // account's storage changes (as a hashmap from slot index to slot value),
            // unless REVM output doesn't contain any storage for this account, in which case
            // we set this field to None. If REVM did return storage, we return one record
            // per *modified* slot (sometimes REVM returns a storage record for an account
            // even if the slots are not modified).
            let mut account_updates: HashMap<Address, StateUpdate> = HashMap::new();
            for (address, account) in state {
                account_updates.insert(
                    address,
                    StateUpdate {
                        // revm doesn't say if the balance was actually changed
                        balance: Some(account.info.balance),
                        // revm doesn't say if the code was actually changed
                        storage: {
                            if account.storage.is_empty() {
                                None
                            } else {
                                let mut slot_updates: HashMap<U256, U256> = HashMap::new();
                                for (index, slot) in account.storage {
                                    if slot.is_changed() {
                                        slot_updates.insert(index, slot.present_value);
                                    }
                                }
                                if slot_updates.is_empty() {
                                    None
                                } else {
                                    Some(slot_updates)
                                }
                            }
                        },
                    },
                );
            }
            account_updates
        },
        gas_used: gas_used - gas_refunded,
        transient_storage,
    }
}

#[derive(Debug)]
/// Data needed to invoke a transaction simulation
pub struct SimulationParameters {
    /// Address of the sending account
    pub caller: Address,
    /// Address of the receiving account/contract
    pub to: Address,
    /// Calldata
    pub data: Vec<u8>,
    /// Amount of native token sent
    pub value: U256,
    /// EVM state overrides.
    /// Will be merged with existing state. Will take effect only for current simulation.
    pub overrides: Option<HashMap<Address, HashMap<U256, U256>>>,
    /// Limit of gas to be used by the transaction
    pub gas_limit: Option<u64>,
    /// Map of the address whose transient storage will be overwritten, to a map of storage slot
    /// and value.
    pub transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    /// Per-call block context overrides.
    pub block_overrides: Option<BlockEnvOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockEnvOverrides {
    pub number: Option<u64>,
    pub timestamp: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::{error::Error, str::FromStr, time::Instant};

    use alloy::{
        primitives::{Address, Bytes, Keccak256, B256},
        sol_types::SolValue,
        transports::{RpcError, TransportError, TransportErrorKind},
    };
    use revm::{
        context::result::{HaltReason, InvalidTransaction, OutOfGasError, SuccessReason},
        state::{
            Account, AccountInfo, AccountStatus, Bytecode, EvmState as rState, EvmStorageSlot,
        },
    };
    use tycho_client::feed::BlockHeader;
    use tycho_common::simulation::errors::SimulationError;

    use super::*;
    use crate::evm::engine_db::{
        engine_db_interface::EngineDatabaseInterface,
        simulation_db::{EVMProvider, SimulationDB},
        utils::{get_client, get_runtime},
    };

    #[test]
    fn test_interpret_result_ok_success() {
        let evm_result: Result<ResultAndState, EVMError<TransportError>> = Ok(ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas_used: 100_u64,
                gas_refunded: 10_u64,
                logs: Vec::new(),
                output: Output::Call(Bytes::from_static(b"output")),
            },
            state: [(
                // storage has changed
                Address::ZERO,
                Account {
                    info: AccountInfo {
                        balance: U256::from_limbs([1, 0, 0, 0]),
                        nonce: 2,
                        code_hash: B256::ZERO,
                        code: None,
                    },
                    transaction_id: 0,
                    storage: [
                        // this slot has changed
                        (
                            U256::from_limbs([3, 1, 0, 0]),
                            EvmStorageSlot {
                                original_value: U256::from_limbs([4, 0, 0, 0]),
                                present_value: U256::from_limbs([5, 0, 0, 0]),
                                transaction_id: 0,
                                is_cold: true,
                            },
                        ),
                        // this slot hasn't changed
                        (
                            U256::from_limbs([3, 2, 0, 0]),
                            EvmStorageSlot {
                                original_value: U256::from_limbs([4, 0, 0, 0]),
                                present_value: U256::from_limbs([4, 0, 0, 0]),
                                transaction_id: 0,
                                is_cold: true,
                            },
                        ),
                    ]
                    .iter()
                    .cloned()
                    .collect(),
                    status: AccountStatus::Touched,
                },
            )]
            .iter()
            .cloned()
            .collect(),
        });

        let transient_storage = HashMap::from([(
            Address::from_str("0x1f98400000000000000000000000000000000004").unwrap(),
            HashMap::from([(U256::from(0), U256::from(1))]),
        )]);
        let result = interpret_evm_result(evm_result, transient_storage.clone());
        let simulation_result = result.unwrap();

        assert_eq!(simulation_result.result, Bytes::from_static(b"output"));
        let expected_state_updates = [(
            Address::ZERO,
            StateUpdate {
                storage: Some(
                    [(U256::from_limbs([3, 1, 0, 0]), U256::from_limbs([5, 0, 0, 0]))]
                        .iter()
                        .cloned()
                        .collect(),
                ),
                balance: Some(U256::from_limbs([1, 0, 0, 0])),
            },
        )]
        .iter()
        .cloned()
        .collect();
        assert_eq!(simulation_result.state_updates, expected_state_updates);
        assert_eq!(simulation_result.gas_used, 90);
        assert_eq!(simulation_result.transient_storage, transient_storage);
    }

    #[test]
    fn test_interpret_result_ok_revert() {
        let evm_result: Result<ResultAndState, EVMError<TransportError>> = Ok(ResultAndState {
            result: ExecutionResult::Revert {
                gas_used: 100_u64,
                output: Bytes::from_static(b"output"),
            },
            state: rState::default(),
        });

        let result = interpret_evm_result(evm_result, HashMap::new());

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationEngineError::TransactionError { data: _, gas_used } => {
                assert_eq!(
                    format!("0x{}", hex::encode::<Vec<u8>>("output".into())),
                    "0x6f7574707574"
                );
                assert_eq!(gas_used, Some(100));
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_ok_halt() {
        let evm_result: Result<ResultAndState, EVMError<TransportError>> = Ok(ResultAndState {
            result: ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(OutOfGasError::Basic),
                gas_used: 100_u64,
            },
            state: rState::default(),
        });

        let result = interpret_evm_result(evm_result, HashMap::new());

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationEngineError::TransactionError { data, gas_used } => {
                assert_eq!(data, "OutOfGas(Basic)");
                assert_eq!(gas_used, Some(100));
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_err_invalid_transaction() {
        let evm_result: Result<ResultAndState, EVMError<TransportError>> =
            Err(EVMError::Transaction(InvalidTransaction::PriorityFeeGreaterThanMaxFee));

        let result = interpret_evm_result(evm_result, HashMap::new());

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationEngineError::TransactionError { data, gas_used } => {
                assert_eq!(data, "EVM error: PriorityFeeGreaterThanMaxFee");
                assert_eq!(gas_used, None);
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_err_db_error() {
        let evm_result: Result<ResultAndState, EVMError<TransportError>> = Err(EVMError::Database(
            RpcError::Transport(TransportErrorKind::Custom(Box::from("boo".to_string()))),
        ));

        let result = interpret_evm_result(evm_result, HashMap::new());

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationEngineError::StorageError(msg) => {
                assert_eq!(msg, "Storage error: Transport(Custom(\"boo\"))")
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }
    fn new_state() -> SimulationDB<EVMProvider> {
        let runtime = get_runtime().expect("Failed to create test runtime");
        let client = get_client(None).expect("Failed to create test client");
        SimulationDB::new(client, runtime, None)
    }

    #[test]
    fn test_simulate_applies_block_env_overrides() -> Result<(), Box<dyn Error>> {
        let mut state = new_state();
        let contract = Address::from_str("0x0000000000000000000000000000000000001234")?;
        // Minimal runtime bytecode equivalent to the following Solidity contract:
        //
        // // SPDX-License-Identifier: UNLICENSED
        // pragma solidity ^0.8.26;
        //
        // contract BlockNumberTest {
        //     function test() external view returns (uint256) {
        //         return block.number;
        //     }
        // }
        let bytecode = Bytecode::new_raw(Bytes::from_static(&[
            0x43, // NUMBER
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
        ]));
        let account = AccountInfo::new(U256::ZERO, 0, bytecode.hash_slow(), bytecode);
        state.init_account(contract, account, None, true)?;
        state.init_account(Address::ZERO, AccountInfo::default(), None, true)?;
        state.set_block(Some(BlockHeader { number: 1, timestamp: 2, ..Default::default() }));

        let sim_params = SimulationParameters {
            caller: Address::ZERO,
            to: contract,
            data: Vec::new(),
            value: U256::ZERO,
            overrides: None,
            gas_limit: None,
            transient_storage: None,
            block_overrides: Some(BlockEnvOverrides { number: Some(123), timestamp: Some(456) }),
        };

        let engine = SimulationEngine::new(state, false);
        let result = engine
            .simulate(&sim_params)
            .expect("simulation should apply block env overrides");

        assert_eq!(U256::from_be_slice(result.result.as_ref()), U256::from(123));
        Ok(())
    }

    #[test]
    fn test_integration_revm_v2_swap() -> Result<(), Box<dyn Error>> {
        let state = new_state();

        // any random address will work
        let caller = Address::from_str("0x0000000000000000000000000000000000000000")?;
        let router_addr = Address::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D")?;
        let weth_addr = Address::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?;
        let usdc_addr = Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?;

        // Define the function selector and input arguments
        let selector = "getAmountsOut(uint256,address[])";
        let amount_in = U256::from(100_000_000);
        let path = vec![usdc_addr, weth_addr];

        let encoded = {
            let args = (amount_in, path);
            let mut hasher = Keccak256::new();
            hasher.update(selector.as_bytes());
            let selector_bytes = &hasher.finalize()[..4];
            let mut data = selector_bytes.to_vec();
            let mut encoded_args = args.abi_encode();
            // Remove extra prefix if present (32 bytes for dynamic data)
            if encoded_args.len() > 32 &&
                encoded_args[..32] ==
                    [0u8; 31]
                        .into_iter()
                        .chain([32].to_vec())
                        .collect::<Vec<u8>>()
            {
                encoded_args = encoded_args[32..].to_vec();
            }
            data.extend(encoded_args);
            data
        };

        // Simulation parameters
        let sim_params = SimulationParameters {
            caller,
            to: router_addr,
            data: encoded,
            value: U256::from(0u64),
            overrides: None,
            gas_limit: None,
            transient_storage: None,
            block_overrides: None,
        };
        let mut eng = SimulationEngine::new(state, true);

        let block = BlockHeader {
            number: 23428552,
            hash: tycho_common::Bytes::from_str(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            timestamp: 1758665355,
            ..Default::default()
        };
        eng.state.set_block(Some(block));

        let result = eng.simulate(&sim_params);
        type BalanceReturn = Vec<U256>;
        let amounts_out: Vec<U256> = match result {
            Ok(SimulationResult { result, .. }) => {
                BalanceReturn::abi_decode(&result).map_err(|e| {
                    SimulationError::FatalError(format!("Failed to decode result: {e:?}"))
                })?
            }
            _ => panic!("Execution reverted!"),
        };

        println!(
            "Swap yielded {} WETH",
            amounts_out
                .last()
                .expect("Empty decoding result")
        );

        let start = Instant::now();
        let n_iter = 1000;
        for _ in 0..n_iter {
            eng.simulate(&sim_params).unwrap();
        }
        let duration = start.elapsed();

        println!("Using revm:");
        println!("Total Duration [n_iter={n_iter}]: {duration:?}");
        println!("Single get_amount_out call: {per_call:?}", per_call = duration / n_iter);

        Ok(())
    }

    #[test]
    fn test_contract_deployment() -> Result<(), Box<dyn Error>> {
        let readonly_state = new_state();
        let state = new_state();

        let selector = "balanceOf(address)";
        let eoa_address = Address::from_str("0xDFd5293D8e347dFe59E90eFd55b2956a1343963d")?;
        let calldata = {
            let args = eoa_address;
            let mut hasher = Keccak256::new();
            hasher.update(selector.as_bytes());
            let selector_bytes = &hasher.finalize()[..4];
            let mut data = selector_bytes.to_vec();
            data.extend(args.abi_encode());
            data
        };

        let usdt_address = Address::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();
        let _ = readonly_state
            .basic_ref(usdt_address)
            .unwrap()
            .unwrap();

        // let deploy_bytecode = std::fs::read(
        //     "/home/mdank/repos/datarevenue/DEFI/defibot-solver/defibot/swaps/pool_state/dodo/
        // compiled/ERC20.bin-runtime" ).unwrap();
        // let deploy_bytecode = revm::precompile::Bytes::from(mocked_bytecode);
        let _ = Bytes::from(hex::decode("608060405234801562000010575f80fd5b5060405162000a6b38038062000a6b83398101604081905262000033916200012c565b600362000041848262000237565b50600462000050838262000237565b506005805460ff191660ff9290921691909117905550620002ff9050565b634e487b7160e01b5f52604160045260245ffd5b5f82601f83011262000092575f80fd5b81516001600160401b0380821115620000af57620000af6200006e565b604051601f8301601f19908116603f01168101908282118183101715620000da57620000da6200006e565b81604052838152602092508683858801011115620000f6575f80fd5b5f91505b83821015620001195785820183015181830184015290820190620000fa565b5f93810190920192909252949350505050565b5f805f606084860312156200013f575f80fd5b83516001600160401b038082111562000156575f80fd5b620001648783880162000082565b945060208601519150808211156200017a575f80fd5b50620001898682870162000082565b925050604084015160ff81168114620001a0575f80fd5b809150509250925092565b600181811c90821680620001c057607f821691505b602082108103620001df57634e487b7160e01b5f52602260045260245ffd5b50919050565b601f82111562000232575f81815260208120601f850160051c810160208610156200020d5750805b601f850160051c820191505b818110156200022e5782815560010162000219565b5050505b505050565b81516001600160401b038111156200025357620002536200006e565b6200026b81620002648454620001ab565b84620001e5565b602080601f831160018114620002a1575f8415620002895750858301515b5f19600386901b1c1916600185901b1785556200022e565b5f85815260208120601f198616915b82811015620002d157888601518255948401946001909101908401620002b0565b5085821015620002ef57878501515f19600388901b60f8161c191681555b5050505050600190811b01905550565b61075e806200030d5f395ff3fe608060405234801561000f575f80fd5b50600436106100a6575f3560e01c8063395093511161006e578063395093511461011f57806370a082311461013257806395d89b411461015a578063a457c2d714610162578063a9059cbb14610175578063dd62ed3e14610188575f80fd5b806306fdde03146100aa578063095ea7b3146100c857806318160ddd146100eb57806323b872dd146100fd578063313ce56714610110575b5f80fd5b6100b261019b565b6040516100bf91906105b9565b60405180910390f35b6100db6100d636600461061f565b61022b565b60405190151581526020016100bf565b6002545b6040519081526020016100bf565b6100db61010b366004610647565b610244565b604051601281526020016100bf565b6100db61012d36600461061f565b610267565b6100ef610140366004610680565b6001600160a01b03165f9081526020819052604090205490565b6100b2610288565b6100db61017036600461061f565b610297565b6100db61018336600461061f565b6102f2565b6100ef6101963660046106a0565b6102ff565b6060600380546101aa906106d1565b80601f01602080910402602001604051908101604052809291908181526020018280546101d6906106d1565b80156102215780601f106101f857610100808354040283529160200191610221565b820191905f5260205f20905b81548152906001019060200180831161020457829003601f168201915b5050505050905090565b5f33610238818585610329565b60019150505b92915050565b5f336102518582856103dc565b61025c85858561043e565b506001949350505050565b5f3361023881858561027983836102ff565b6102839190610709565b610329565b6060600480546101aa906106d1565b5f33816102a482866102ff565b9050838110156102e557604051632983c0c360e21b81526001600160a01b038616600482015260248101829052604481018590526064015b60405180910390fd5b61025c8286868403610329565b5f3361023881858561043e565b6001600160a01b039182165f90815260016020908152604080832093909416825291909152205490565b6001600160a01b0383166103525760405163e602df0560e01b81525f60048201526024016102dc565b6001600160a01b03821661037b57604051634a1406b160e11b81525f60048201526024016102dc565b6001600160a01b038381165f8181526001602090815260408083209487168084529482529182902085905590518481527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b92591015b60405180910390a3505050565b5f6103e784846102ff565b90505f198114610438578181101561042b57604051637dc7a0d960e11b81526001600160a01b038416600482015260248101829052604481018390526064016102dc565b6104388484848403610329565b50505050565b6001600160a01b03831661046757604051634b637e8f60e11b81525f60048201526024016102dc565b6001600160a01b0382166104905760405163ec442f0560e01b81525f60048201526024016102dc565b61049b8383836104a0565b505050565b6001600160a01b0383166104ca578060025f8282546104bf9190610709565b9091555061053a9050565b6001600160a01b0383165f908152602081905260409020548181101561051c5760405163391434e360e21b81526001600160a01b038516600482015260248101829052604481018390526064016102dc565b6001600160a01b0384165f9081526020819052604090209082900390555b6001600160a01b03821661055657600280548290039055610574565b6001600160a01b0382165f9081526020819052604090208054820190555b816001600160a01b0316836001600160a01b03167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef836040516103cf91815260200190565b5f6020808352835180828501525f5b818110156105e4578581018301518582016040015282016105c8565b505f604082860101526040601f19601f8301168501019250505092915050565b80356001600160a01b038116811461061a575f80fd5b919050565b5f8060408385031215610630575f80fd5b61063983610604565b946020939093013593505050565b5f805f60608486031215610659575f80fd5b61066284610604565b925061067060208501610604565b9150604084013590509250925092565b5f60208284031215610690575f80fd5b61069982610604565b9392505050565b5f80604083850312156106b1575f80fd5b6106ba83610604565b91506106c860208401610604565b90509250929050565b600181811c90821680620001c057607f821691505b602082108103620001df57634e487b7160e01b5f52602260045260245ffd5b50919050565b8082018082111561023e57634e487b7160e01b5f52601160045260245ffdfea2646970667358221220dfc123d5852c9246ea16b645b377b4436e2f778438195cc6d6c435e8c73a20e764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000000000")?);

        let onchain_bytecode = Bytes::from(hex::decode("608060405234801561000f575f80fd5b50600436106100a6575f3560e01c8063395093511161006e578063395093511461011f57806370a082311461013257806395d89b411461015a578063a457c2d714610162578063a9059cbb14610175578063dd62ed3e14610188575f80fd5b806306fdde03146100aa578063095ea7b3146100c857806318160ddd146100eb57806323b872dd146100fd578063313ce56714610110575b5f80fd5b6100b261019b565b6040516100bf91906105b9565b60405180910390f35b6100db6100d636600461061f565b61022b565b60405190151581526020016100bf565b6002545b6040519081526020016100bf565b6100db61010b366004610647565b610244565b604051601281526020016100bf565b6100db61012d36600461061f565b610267565b6100ef610140366004610680565b6001600160a01b03165f9081526020819052604090205490565b6100b2610288565b6100db61017036600461061f565b610297565b6100db61018336600461061f565b6102f2565b6100ef6101963660046106a0565b6102ff565b6060600380546101aa906106d1565b80601f01602080910402602001604051908101604052809291908181526020018280546101d6906106d1565b80156102215780601f106101f857610100808354040283529160200191610221565b820191905f5260205f20905b81548152906001019060200180831161020457829003601f168201915b5050505050905090565b5f33610238818585610329565b60019150505b92915050565b5f336102518582856103dc565b61025c85858561043e565b506001949350505050565b5f3361023881858561027983836102ff565b6102839190610709565b610329565b6060600480546101aa906106d1565b5f33816102a482866102ff565b9050838110156102e557604051632983c0c360e21b81526001600160a01b038616600482015260248101829052604481018590526064015b60405180910390fd5b61025c8286868403610329565b5f3361023881858561043e565b6001600160a01b039182165f90815260016020908152604080832093909416825291909152205490565b6001600160a01b0383166103525760405163e602df0560e01b81525f60048201526024016102dc565b6001600160a01b03821661037b57604051634a1406b160e11b81525f60048201526024016102dc565b6001600160a01b038381165f8181526001602090815260408083209487168084529482529182902085905590518481527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b92591015b60405180910390a3505050565b5f6103e784846102ff565b90505f198114610438578181101561042b57604051637dc7a0d960e11b81526001600160a01b038416600482015260248101829052604481018390526064016102dc565b6104388484848403610329565b50505050565b6001600160a01b03831661046757604051634b637e8f60e11b81525f60048201526024016102dc565b6001600160a01b0382166104905760405163ec442f0560e01b81525f60048201526024016102dc565b61049b8383836104a0565b505050565b6001600160a01b0383166104ca578060025f8282546104bf9190610709565b9091555061053a9050565b6001600160a01b0383165f908152602081905260409020548181101561051c5760405163391434e360e21b81526001600160a01b038516600482015260248101829052604481018390526064016102dc565b6001600160a01b0384165f9081526020819052604090209082900390555b6001600160a01b03821661055657600280548290039055610574565b6001600160a01b0382165f9081526020819052604090208054820190555b816001600160a01b0316836001600160a01b03167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef836040516103cf91815260200190565b5f6020808352835180828501525f5b818110156105e4578581018301518582016040015282016105c8565b505f604082860101526040601f19601f8301168501019250505092915050565b80356001600160a01b038116811461061a575f80fd5b919050565b5f8060408385031215610630575f80fd5b61063983610604565b946020939093013593505050565b5f805f60608486031215610659575f80fd5b61066284610604565b925061067060208501610604565b9150604084013590509250925092565b5f60208284031215610690575f80fd5b61069982610604565b9392505050565b5f80604083850312156106b1575f80fd5b6106ba83610604565b91506106c860208401610604565b90509250929050565b600181811c908216806106e557607f821691505b60208210810361070357634e487b7160e01b5f52602260045260245ffd5b50919050565b8082018082111561023e57634e487b7160e01b5f52601160045260245ffdfea2646970667358221220dfc123d5852c9246ea16b645b377b4436e2f778438195cc6d6c435e8c73a20e764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000000000")?);
        let code = Bytecode::new_raw(onchain_bytecode);
        let contract_acc_info = AccountInfo::new(
            U256::from(0),
            0,
            code.hash_slow(),
            code,
            // true_usdt.code.unwrap(),
        );
        // Adding permanent storage for balance
        let mut storage = HashMap::default();
        storage.insert(
            U256::from_str(
                "25842306973167774731510882590667189188844731550465818811072464953030320818263",
            )
            .unwrap(),
            U256::from_str("25").unwrap(),
        );
        // MOCK A BALANCE AND APPROVAL
        // let mut permanent_storage = HashMap::new();
        // permanent_storage.insert(s)
        state
            .init_account(usdt_address, contract_acc_info, Some(storage), true)
            .expect("Failed to init account");

        // DEPLOY A CONTRACT TO GET ON-CHAIN BYTECODE
        // let deployment_account = B160::from_str("0x0000000000000000000000000000000000000123")?;
        // state.init_account(
        //     deployment_account,
        //     AccountInfo::new(U256::MAX, 0, Bytecode::default()),
        //     None,
        //     true,
        // );
        // let deployment_params = SimulationParameters {
        //     caller: Address::from(deployment_account),
        //     to: Address::zero(),
        //     data: Bytes::from(deploy_bytecode),
        //     value: U256::from(0u64),
        //     overrides: None,
        //     gas_limit: None,
        // };

        // prepare balanceOf
        // let deployed_contract_address =
        // B160::from_str("0x5450b634edf901a95af959c99c058086a51836a8")?; Adding overwrite
        // for balance
        let mut overrides = HashMap::default();
        let mut storage_overwrite = HashMap::default();
        storage_overwrite.insert(
            U256::from_str(
                "25842306973167774731510882590667189188844731550465818811072464953030320818263",
            )
            .unwrap(),
            U256::from_str("80").unwrap(),
        );
        overrides.insert(usdt_address, storage_overwrite);

        let sim_params = SimulationParameters {
            caller: Address::from_str("0x0000000000000000000000000000000000000000")?,
            to: usdt_address,
            // to: Address::from(deployed_contract_address),
            data: calldata,
            value: U256::from(0u64),
            overrides: Some(overrides),
            gas_limit: None,
            transient_storage: None,
            block_overrides: None,
        };

        let mut eng = SimulationEngine::new(state, false);

        // Dummy block (irrelevant for this test)
        let block = BlockHeader {
            number: 1,
            hash: tycho_common::Bytes::from_str(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            timestamp: 1748397011,
            ..Default::default()
        };
        eng.state.set_block(Some(block));

        // println!("Deploying a mocked contract!");
        // let deployment_result = eng.simulate(&deployment_params);
        // match deployment_result {
        //     Ok(SimulationResult { result, state_updates, gas_used }) => {
        //         println!("Deployment result: {:?}", result);
        //         println!("Used gas: {:?}", gas_used);
        //         println!("{:?}", state_updates);
        //     }
        //     Err(error) => panic!("{:?}", error),
        // };

        println!("Executing balanceOf");
        let result = eng.simulate(&sim_params);
        let balance = match result {
            Ok(SimulationResult { result, .. }) => U256::abi_decode(&result)?,
            Err(error) => panic!("{error:?}"),
        };
        println!("Balance: {balance}");

        Ok(())
    }
}
