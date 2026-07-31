use alloy::{
    primitives::map::AddressHashMap,
    rpc::types::{state::AccountOverride, TransactionRequest},
};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::Bytes;
use tycho_execution::encoding::models::Solution;

use crate::execution::tenderly::OverwriteMetadata;

/// Information required to execute a Tycho transaction simulation.
/// Contains the solution data, transaction details, and data needed for verification.
#[derive(Debug, Clone)]
pub struct TychoExecutionInput {
    pub solution: Solution,
    pub transaction: Transaction,
    pub expected_amount_out: BigUint,
    pub protocol_system: String,
    pub component_id: String,
    pub token_in: String,
    pub token_out: String,
    pub estimated_gas: BigUint,
}

/// Result of executing a Tycho transaction simulation.
/// Represents the three possible outcomes: successful execution, transaction revert, or execution
/// failure.
#[derive(Clone)]
pub enum TychoExecutionResult {
    /// Successful execution with output amount and gas consumption
    Success {
        amount_out: BigUint,
        gas_used: u64,
        state_overwrites: Option<AddressHashMap<AccountOverride>>,
        overwrite_metadata: Option<OverwriteMetadata>,
    },
    /// Simulation reverted with reason and optional state overrides for debugging
    Revert {
        reason: String,
        state_overwrites: Option<AddressHashMap<AccountOverride>>,
        overwrite_metadata: Option<OverwriteMetadata>,
    },
    /// Execution failed due to error during preparation or processing
    Failed { error_msg: String },
}

/// Input parameters for simulating a transaction.
/// Contains the transaction request and optional state modifications.
#[derive(Debug, Clone)]
pub(super) struct SimulationInput {
    pub tx: TransactionRequest,
    pub state_overwrites: Option<AddressHashMap<AccountOverride>>,
    pub overwrite_metadata: Option<OverwriteMetadata>,
}

/// Result of a transaction simulation with execution trace.
/// Contains either successful execution data or revert information.
#[derive(Debug, Clone)]
pub(super) enum SimulationResult {
    /// Successful simulation with return data and gas consumption
    Success { return_data: Vec<u8>, gas_used: u64 },
    /// Simulation reverted with reason
    Revert { reason: String },
}

/// Contains the bytecode required to set up router and executor overwrites for swap simulation.
///
/// This struct packages the bytecode for both the Tycho router contract and the protocol-specific
/// executor contract that will be used during execution simulation. The bytecode is loaded from
/// embedded JSON files and used to override contract code at specific addresses during simulation.
///
/// # Fields
/// * `router_bytecode` - The runtime bytecode for the Tycho router contract
/// * `executor_bytecode` - The runtime bytecode for the protocol-specific executor contract
#[derive(Clone)]
pub struct RouterOverwritesData {
    pub router_bytecode: Vec<u8>,
    pub executor_bytecode: Vec<u8>,
    pub fee_calculator_bytecode: Vec<u8>,
}

/// An encoded EVM transaction ready to be submitted on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Contract address to call.
    to: Bytes,
    /// Native token value to send with the transaction.
    value: BigUint,
    /// ABI-encoded calldata.
    data: Vec<u8>,
    /// Estimated gas usage for this transaction
    estimated_gas: BigUint,
}

impl Transaction {
    /// Creates a new transaction.
    pub fn new(to: Bytes, value: BigUint, data: Vec<u8>, estimated_gas: BigUint) -> Self {
        Self { to, value, data, estimated_gas }
    }

    /// Returns the contract address to call.
    pub fn to(&self) -> &Bytes {
        &self.to
    }

    /// Returns the native token value to send.
    pub fn value(&self) -> &BigUint {
        &self.value
    }

    /// Returns the ABI-encoded calldata.
    pub fn data(&self) -> &Vec<u8> {
        &self.data
    }

    /// Returns the estimated gas usage
    pub fn estimated_gas(&self) -> &BigUint {
        &self.estimated_gas
    }
}
