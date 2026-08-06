use std::{collections::HashMap, env, fmt::Debug, str::FromStr};

use alloy::{
    primitives::{Address, Bytes, Keccak256, U256},
    providers::{Provider, ProviderBuilder},
    sol_types::SolValue,
    transports::{RpcError, TransportErrorKind},
};
use hex::FromHex;
use num_bigint::BigInt;
use revm::{
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use serde_json::Value;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    simulation::{SimulationEngine, SimulationEngineError, SimulationParameters},
    ContractCompiler, SlotId,
};

pub(crate) fn coerce_error(
    err: &SimulationEngineError,
    pool_state: &str,
    gas_limit: Option<u64>,
) -> SimulationError {
    match err {
        // Check for revert situation (if error message starts with "0x")
        SimulationEngineError::TransactionError { ref data, ref gas_used }
            if data.starts_with("0x") =>
        {
            let reason = parse_solidity_error_message(data);
            let err = SimulationEngineError::TransactionError {
                data: format!("Revert! Reason: {reason}"),
                gas_used: *gas_used,
            };

            // Check if we are running out of gas
            if let (Some(gas_limit), Some(gas_used)) = (gas_limit, gas_used) {
                // if we used up 97% or more issue a OutOfGas error.
                let usage = *gas_used as f64 / gas_limit as f64;
                if usage >= 0.97 {
                    return SimulationError::InvalidInput(
                        format!(
                            "SimulationError: Likely out-of-gas. Used: {:.2}% of gas limit. \
                            Original error: {}. \
                            Pool state: {}",
                            usage * 100.0,
                            err,
                            pool_state,
                        ),
                        None,
                    );
                }
            }
            SimulationError::FatalError(format!("Simulation reverted for unknown reason: {reason}"))
        }
        // Check if "OutOfGas" is part of the error message
        SimulationEngineError::TransactionError { ref data, ref gas_used }
            if data.contains("OutOfGas") =>
        {
            let usage_msg = if let (Some(gas_limit), Some(gas_used)) = (gas_limit, gas_used) {
                let usage = *gas_used as f64 / gas_limit as f64;
                format!("Used: {:.2}% of gas limit. ", usage * 100.0)
            } else {
                String::new()
            };

            SimulationError::InvalidInput(
                format!(
                    "SimulationError: out-of-gas. {usage_msg} Original error: {data}. Pool state: {pool_state}"
                ),
                None,
            )
        }
        SimulationEngineError::TransactionError { ref data, .. } => {
            SimulationError::FatalError(format!("TransactionError: {data}"))
        }
        SimulationEngineError::StorageError(message) => {
            SimulationError::RecoverableError(message.clone())
        }
        _ => SimulationError::FatalError(err.clone().to_string()), /* Otherwise return the
                                                                    * original error */
    }
}

fn parse_solidity_error_message(data: &str) -> String {
    // 10 for "0x" + 8 hex chars error signature
    if data.len() >= 10 {
        let data_bytes = match Vec::from_hex(&data[2..]) {
            Ok(bytes) => bytes,
            Err(_) => return format!("Failed to decode: {data}"),
        };

        // Check for specific error selectors:
        // Solidity Error(string) signature: 0x08c379a0
        if data_bytes.starts_with(&[0x08, 0xc3, 0x79, 0xa0]) {
            if let Ok(decoded) = String::abi_decode(&data_bytes[4..]) {
                return decoded;
            }

            // Solidity Panic(uint256) signature: 0x4e487b71
        } else if data_bytes.starts_with(&[0x4e, 0x48, 0x7b, 0x71]) {
            if let Ok(decoded) = U256::abi_decode(&data_bytes[4..]) {
                let panic_codes = get_solidity_panic_codes();
                return panic_codes
                    .get(&decoded.as_limbs()[0])
                    .cloned()
                    .unwrap_or_else(|| format!("Panic({decoded})"));
            }
        }

        // Try decoding as a string (old Solidity revert case)
        if let Ok(decoded) = String::abi_decode(&data_bytes) {
            return decoded;
        }

        // Custom error, try to decode string again with offset
        if let Ok(decoded) = String::abi_decode(&data_bytes[4..]) {
            return decoded;
        }
    }
    // Fallback if no decoding succeeded
    format!("Failed to decode: {data}")
}

/// Get storage slot index of a value stored at a certain key in a mapping
///
/// # Arguments
///
/// * `key`: Key in a mapping. Can be any H160 value (such as an address).
/// * `mapping_slot`: An `U256` representing the storage slot at which the mapping itself is stored.
///   See the examples for more explanation.
/// * `compiler`: The compiler with which the target contract was compiled. Solidity and Vyper
///   handle maps differently.
///
/// # Returns
///
/// An `U256` representing the  index of a storage slot where the value at the given
/// key is stored.
///
/// # Examples
///
/// If a mapping is declared as a first variable in Solidity code, its storage slot
/// is 0 (e.g. `balances` in our mocked ERC20 contract). Here's how to compute
/// a storage slot where balance of a given account is stored:
///
/// ```
/// use alloy::primitives::{U256, Address};
/// use tycho_simulation::evm::ContractCompiler;
/// use tycho_simulation::evm::protocol::vm::utils::get_storage_slot_index_at_key;
/// let address: Address = "0xC63135E4bF73F637AF616DFd64cf701866BB2628".parse().expect("Invalid address");
/// get_storage_slot_index_at_key(address, U256::from(0), ContractCompiler::Solidity);
/// ```
///
/// For nested mappings, we need to apply the function twice. An example of this is
/// `allowances` in ERC20. It is a mapping of form:
/// `HashMap<Owner, HashMap<Spender, U256>>`. In our mocked ERC20 contract, `allowances`
/// is a second variable, so it is stored at slot 1. Here's how to get a storage slot
/// where an allowance of `address_spender` to spend `address_owner`'s money is stored:
///
/// ```
/// use alloy::primitives::{U256, Address};
/// use tycho_simulation::evm::ContractCompiler;
/// use tycho_simulation::evm::protocol::vm::utils::get_storage_slot_index_at_key;
/// let address_spender: Address = "0xC63135E4bF73F637AF616DFd64cf701866BB2628".parse().expect("Invalid address");
/// let address_owner: Address = "0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05".parse().expect("Invalid address");
/// get_storage_slot_index_at_key(address_spender, get_storage_slot_index_at_key(address_owner, U256::from(1), ContractCompiler::Solidity), ContractCompiler::Solidity);
/// ```
///
/// # See Also
///
/// [Solidity Storage Layout documentation](https://docs.soliditylang.org/en/v0.8.13/internals/layout_in_storage.html#mappings-and-dynamic-arrays)
pub fn get_storage_slot_index_at_key(
    key: Address,
    mapping_slot: SlotId,
    compiler: ContractCompiler,
) -> SlotId {
    let mut key_bytes = key.as_slice().to_vec();
    if key_bytes.len() < 32 {
        let padding = vec![0u8; 32 - key_bytes.len()];
        key_bytes.splice(0..0, padding); // Prepend zeros to the start
    }

    let mapping_slot_bytes: [u8; 32] = mapping_slot.to_be_bytes();
    compiler.compute_map_slot(&mapping_slot_bytes, &key_bytes)
}

fn get_solidity_panic_codes() -> HashMap<u64, String> {
    let mut panic_codes = HashMap::new();
    panic_codes.insert(0, "GenericCompilerPanic".to_string());
    panic_codes.insert(1, "AssertionError".to_string());
    panic_codes.insert(17, "ArithmeticOver/Underflow".to_string());
    panic_codes.insert(18, "ZeroDivisionError".to_string());
    panic_codes.insert(33, "UnknownEnumMember".to_string());
    panic_codes.insert(34, "BadStorageByteArrayEncoding".to_string());
    panic_codes.insert(51, "EmptyArray".to_string());
    panic_codes.insert(0x32, "OutOfBounds".to_string());
    panic_codes.insert(0x41, "OutOfMemory".to_string());
    panic_codes.insert(0x51, "BadFunctionPointer".to_string());
    panic_codes
}

/// Fetches the bytecode for a specified contract address, returning an error if the address is
/// an Externally Owned Account (EOA) or if no code is associated with it.
///
/// This function checks the specified address on the blockchain, attempting to retrieve any
/// contract bytecode deployed at that address. If the address corresponds to an EOA or any
/// other address without associated bytecode, an `RpcError::EmptyResponse` error is returned.
///
/// # Parameters
/// - `address`: The address of the account or contract to query, as a string.
/// - `connection_string`: An optional RPC connection string. If not provided, the function will
///   default to the `RPC_URL` environment variable.
///
/// # Returns
/// - `Ok(Bytecode)`: The bytecode of the contract at the specified address, if present.
/// - `Err(RpcError)`: An error if the address does not have associated bytecode, if there is an
///   issue with the RPC connection, or if the address is invalid.
///
/// # Errors
/// - Returns `RpcError::InvalidRequest` if `address` is not parsable or if no RPC URL is set.
/// - Returns `RpcError::EmptyResponse` if the address has no associated bytecode (e.g., EOA).
/// - Returns `RpcError::InvalidResponse` for issues with the RPC provider response.
pub(crate) async fn get_code_for_contract(
    address: &str,
    connection_string: Option<String>,
) -> Result<Bytecode, SimulationError> {
    // Get the connection string, defaulting to the RPC_URL environment variable
    let connection_string = connection_string.or_else(|| env::var("RPC_URL").ok());

    let connection_string = match connection_string {
        Some(url) => url,
        None => {
            return Err(SimulationError::FatalError(
                "RPC_URL environment variable is not set".to_string(),
            ))
        }
    };

    let addr = Address::from_str(address)
        .map_err(|_| SimulationError::FatalError(format!("Invalid address format: {address}")))?;
    // Call eth_getCode to get the bytecode of the contract
    match sync_get_code(&connection_string, addr) {
        Ok(code) if code.is_empty() => {
            Err(SimulationError::FatalError("Empty code response from RPC".to_string()))
        }
        Ok(code) => {
            let bytecode = Bytecode::new_raw(Bytes::from(code.to_vec()));
            Ok(bytecode)
        }
        Err(e) => match e {
            RpcError::Transport(err) => Err(SimulationError::RecoverableError(format!(
                "Failed to get code for contract due to internal RPC error: {err:?}"
            ))),
            _ => Err(SimulationError::FatalError(format!(
                "Failed to get code for contract. Invalid response from RPC: {e:?}"
            ))),
        },
    }
}

fn sync_get_code(
    connection_string: &str,
    addr: Address,
) -> Result<Bytes, RpcError<TransportErrorKind>> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Create a provider with the URL
            let provider = ProviderBuilder::new()
                .connect(connection_string)
                .await?;
            provider.get_code_at(addr).await
        })
    })
}

/// Converts a hexadecimal string into a fixed-size 32-byte array.
///
/// This function takes a string slice (e.g., a pool ID) that may or may not have
/// a `0x` prefix. It decodes the hex string into bytes, ensuring it does not exceed
/// 32 bytes in length. If the string is valid and fits within 32 bytes, the bytes
/// are copied into a `[u8; 32]` array, with right zero-padding for unused bytes.
///
/// # Arguments
///
/// * `pool_id` - A string slice representing a hexadecimal pool ID. It can optionally start with
///   the `0x` prefix.
///
/// # Returns
///
/// * `Ok([u8; 32])` - On success, returns a 32-byte array with the decoded bytes. If the input is
///   shorter than 32 bytes, the rest of the array is right padded with zeros.
/// * `Err(SimulationError)` - Returns an error if:
///     - The input string is not a valid hexadecimal string.
///     - The decoded bytes exceed 32 bytes in length.
///
/// # Example
/// ```
/// use tycho_simulation::evm::protocol::vm::utils::string_to_bytes32;
///
/// let pool_id = "0x1234abcd";
/// match string_to_bytes32(pool_id) {
///     Ok(bytes32) => println!("Bytes32: {:?}", bytes32),
///     Err(e) => eprintln!("Error: {}", e),
/// }
pub fn string_to_bytes32(pool_id: &str) -> Result<[u8; 32], SimulationError> {
    let pool_id_no_prefix =
        if let Some(stripped) = pool_id.strip_prefix("0x") { stripped } else { pool_id };
    let bytes = hex::decode(pool_id_no_prefix)
        .map_err(|e| SimulationError::FatalError(format!("Invalid hex string: {e}")))?;
    if bytes.len() > 32 {
        return Err(SimulationError::FatalError(format!(
            "Hex string exceeds 32 bytes: length {}",
            bytes.len()
        )));
    }
    let mut array = [0u8; 32];
    array[..bytes.len()].copy_from_slice(&bytes);
    Ok(array)
}

/// Decodes a JSON-encoded list of hexadecimal strings into a `Vec<Vec<u8>>`.
///
/// This function parses a JSON array where each element is a string representing a hexadecimal
/// value. It converts each hex string into a vector of bytes (`Vec<u8>`), and aggregates them into
/// a `Vec<Vec<u8>>`.
///
/// # Arguments
///
/// * `input` - A byte slice (`&[u8]`) containing JSON-encoded data. The JSON must be a valid array
///   of hex strings.
///
/// # Returns
///
/// * `Ok(Vec<Vec<u8>>)` - On success, returns a vector of byte vectors.
/// * `Err(SimulationError)` - Returns an error if:
///     - The input is not valid JSON.
///     - The JSON is not an array.
///     - Any array element is not a string.
///     - Any string is not a valid hexadecimal string.
///
/// # Example
/// ```
/// use tycho_simulation::evm::protocol::vm::utils::json_deserialize_address_list;
///
/// let json_input = br#"["0x1234", "0xc0ffee"]"#;
/// match json_deserialize_address_list(json_input) {
///     Ok(result) => println!("Decoded: {:?}", result),
///     Err(e) => eprintln!("Error: {}", e),
/// }
/// ```
pub fn json_deserialize_address_list(input: &[u8]) -> Result<Vec<Vec<u8>>, SimulationError> {
    let json_value: Value = serde_json::from_slice(input)
        .map_err(|_| SimulationError::FatalError(format!("Invalid JSON: {input:?}")))?;

    if let Value::Array(hex_strings) = json_value {
        let mut result = Vec::new();

        for val in hex_strings {
            if let Value::String(hexstring) = val {
                let bytes = hex::decode(hexstring.trim_start_matches("0x")).map_err(|_| {
                    SimulationError::FatalError(format!("Invalid hex string: {hexstring}"))
                })?;
                result.push(bytes);
            } else {
                return Err(SimulationError::FatalError("Array contains a non-string value".into()));
            }
        }

        Ok(result)
    } else {
        Err(SimulationError::FatalError("Input is not a JSON array".into()))
    }
}

/// Decodes a JSON-encoded list of hexadecimal strings into a `Vec<BigInt>`.
///
/// This function parses a JSON array where each element is a string representing a hexadecimal
/// value. It converts each hex string into a `BigInt` using big-endian byte interpretation, and
/// aggregates them into a `Vec<BigInt>`.
///
/// # Arguments
///
/// * `input` - A byte slice (`&[u8]`) containing JSON-encoded data. The JSON must be a valid array
///   of hex strings.
///
/// # Returns
///
/// * `Ok(Vec<BigInt>)` - On success, returns a vector of `BigInt` values.
/// * `Err(SimulationError)` - Returns an error if:
///     - The input is not valid JSON.
///     - The JSON is not an array.
///     - Any array element is not a string.
///     - Any string is not a valid hexadecimal string.
///
/// # Example
/// ```
/// use tycho_simulation::evm::protocol::vm::utils::json_deserialize_be_bigint_list;
/// use num_bigint::BigInt;
/// use tycho_simulation::evm;
/// let json_input = br#"["0x1234", "0xdeadbeef"]"#;
/// match json_deserialize_be_bigint_list(json_input) {
///     Ok(result) => println!("Decoded BigInts: {:?}", result),
///     Err(e) => eprintln!("Error: {}", e),
/// }
/// ```
pub fn json_deserialize_be_bigint_list(input: &[u8]) -> Result<Vec<BigInt>, SimulationError> {
    let json_value: Value = serde_json::from_slice(input)
        .map_err(|_| SimulationError::FatalError(format!("Invalid JSON: {input:?}")))?;

    if let Value::Array(hex_strings) = json_value {
        let mut result = Vec::new();

        for val in hex_strings {
            if let Value::String(hexstring) = val {
                let bytes = hex::decode(hexstring.trim_start_matches("0x")).map_err(|_| {
                    SimulationError::FatalError(format!("Invalid hex string: {hexstring}"))
                })?;
                let bigint = BigInt::from_signed_bytes_be(&bytes);
                result.push(bigint);
            } else {
                return Err(SimulationError::FatalError("Array contains a non-string value".into()));
            }
        }

        Ok(result)
    } else {
        Err(SimulationError::FatalError("Input is not a JSON array".into()))
    }
}

/// Load the pool's stateless/implementation contracts (the `stateless_contract_addr_{i}` state
/// attributes) into the engine's DB so getter calls on proxy pools resolve their delegatecall
/// targets. Curve pools are commonly EIP-1167 proxies whose implementation code is not part of the
/// indexed pool storage; without this, getters delegatecall into empty code and revert.
///
/// Addresses may be static (`0x…`) or dynamic (`call:0x<factory>:method()`), and code is fetched
/// via RPC unless provided inline as `stateless_contract_code_{i}`. The engine shares its DB with
/// `SHARED_TYCHO_DB`, so accounts loaded here persist for later `delta_transition` rebuilds.
pub(crate) async fn load_stateless_contracts<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    attributes: &HashMap<String, tycho_common::Bytes>,
) -> Result<(), SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let mut index = 0;
    while let Some(encoded) = attributes.get(&format!("stateless_contract_addr_{index}")) {
        let address = String::from_utf8(encoded.to_vec()).map_err(|e| {
            SimulationError::FatalError(format!("stateless contract address is not UTF-8: {e}"))
        })?;
        let inline_code = attributes
            .get(&format!("stateless_contract_code_{index}"))
            .map(|value| value.to_vec());
        index += 1;

        let (account, code) = match inline_code {
            Some(bytecode) => (address, Bytecode::new_raw(bytecode.into())),
            None => {
                let resolved = if address.starts_with("call") {
                    resolve_call_address(engine, &address)?
                } else {
                    address
                };
                let code = get_code_for_contract(&resolved, None).await?;
                (resolved, code)
            }
        };
        let account: Address = account.parse().map_err(|_| {
            SimulationError::FatalError(format!(
                "stateless contract has an invalid address {account}"
            ))
        })?;
        engine
            .state
            .init_account(
                account,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: code.hash_slow(),
                    code: Some(code),
                },
                None,
                false,
            )
            .map_err(|e| {
                SimulationError::FatalError(format!(
                    "stateless contract init_account failed: {e:?}"
                ))
            })?;
    }
    Ok(())
}

/// Resolve a dynamic `call:0x<address>:method()` directive to a concrete implementation address by
/// simulating the parameterless `method()` view and decoding its returned address.
pub(crate) fn resolve_call_address<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    directive: &str,
) -> Result<String, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let method = directive
        .split(':')
        .next_back()
        .ok_or_else(|| {
            SimulationError::FatalError(format!("malformed stateless call directive {directive}"))
        })?;
    let to: Address = directive
        .split(':')
        .nth(1)
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "stateless call directive is missing its target {directive}"
            ))
        })?
        .parse()
        .map_err(|_| {
            SimulationError::FatalError(format!(
                "stateless call directive has an invalid target {directive}"
            ))
        })?;
    let mut hasher = Keccak256::new();
    hasher.update(method.as_bytes());
    let selector = hasher.finalize()[..4].to_vec();
    let res = engine
        .simulate(&SimulationParameters {
            caller: Address::ZERO,
            to,
            data: selector,
            value: U256::ZERO,
            overrides: None,
            gas_limit: None,
            transient_storage: None,
            block_overrides: None,
        })
        .map_err(|e| SimulationError::FatalError(format!("stateless call failed: {e}")))?;
    let address = Address::abi_decode(res.result.as_ref())
        .map_err(|e| SimulationError::FatalError(format!("stateless call decode failed: {e}")))?;
    Ok(address.to_string())
}

#[cfg(test)]
mod tests {
    use dotenv::dotenv;

    use super::*;
    use crate::utils::hexstring_to_vec;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(not(feature = "network_tests"), ignore)]
    async fn test_get_code_for_address() {
        let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| {
            dotenv().expect("Missing .env file");
            env::var("RPC_URL").expect("Missing RPC_URL in .env file")
        });

        let address = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640";
        let result = get_code_for_contract(address, Some(rpc_url)).await;

        assert!(result.is_ok(), "Network call should not fail");

        let code = result.unwrap();
        assert!(!code.bytes().is_empty(), "Code should not be empty");
    }

    #[test]
    fn test_maybe_coerce_error_revert_no_gas_info() {
        let err = SimulationEngineError::TransactionError{
            data: "0x08c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000011496e76616c6964206f7065726174696f6e000000000000000000000000000000".to_string(),
            gas_used: None
        };

        let result = coerce_error(&err, "test_pool", None);

        if let SimulationError::FatalError(msg) = result {
            assert!(msg.contains("Simulation reverted for unknown reason: Invalid operation"));
        } else {
            panic!("Expected SolidityError error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_out_of_gas() {
        // Test out-of-gas situation with gas limit and gas used provided
        let err = SimulationEngineError::TransactionError{
            data: "0x08c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000011496e76616c6964206f7065726174696f6e000000000000000000000000000000".to_string(),
            gas_used: Some(980)
        };

        let result = coerce_error(&err, "test_pool", Some(1000));

        if let SimulationError::InvalidInput(message, _partial_result) = result {
            assert!(message.contains("Used: 98.00% of gas limit."));
            assert!(message.contains("test_pool"));
        } else {
            panic!("Expected OutOfGas error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_no_gas_limit_info() {
        // Test out-of-gas situation without gas limit info
        let err = SimulationEngineError::TransactionError {
            data: "OutOfGas".to_string(),
            gas_used: None,
        };

        let result = coerce_error(&err, "test_pool", None);

        if let SimulationError::InvalidInput(message, _partial_result) = result {
            assert!(message.contains("Original error: OutOfGas"));
            assert!(message.contains("Pool state: test_pool"));
        } else {
            panic!("Expected RetryDifferentInput error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_storage_error() {
        let err = SimulationEngineError::StorageError("Storage error:".to_string());

        let result = coerce_error(&err, "test_pool", None);

        if let SimulationError::RecoverableError(message) = result {
            assert_eq!(message, "Storage error:");
        } else {
            println!("{result:?}");
            panic!("Expected RetryLater error");
        }
    }

    #[test]
    fn test_maybe_coerce_error_no_match() {
        // Test for non-revert, non-out-of-gas, non-storage errors
        let err = SimulationEngineError::TransactionError {
            data: "Some other error".to_string(),
            gas_used: None,
        };

        let result = coerce_error(&err, "test_pool", None);

        if let SimulationError::FatalError(message) = result {
            assert_eq!(message, "TransactionError: Some other error");
        } else {
            panic!("Expected solidity error");
        }
    }

    #[test]
    fn test_parse_solidity_error_message_error_string() {
        // Test parsing Solidity Error(string) message
        let data = "0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e416d6f756e7420746f6f206c6f77000000000000000000000000000000000000";

        let result = parse_solidity_error_message(data);

        assert_eq!(result, "Amount too low");
    }

    #[test]
    fn test_parse_solidity_error_message_panic_code() {
        // Test parsing Solidity Panic(uint256) message
        let data = "0x4e487b710000000000000000000000000000000000000000000000000000000000000001";

        let result = parse_solidity_error_message(data);

        assert_eq!(result, "AssertionError");
    }

    #[test]
    fn test_parse_solidity_error_message_failed_to_decode() {
        // Test failed decoding with invalid data
        let data = "0x1234567890";

        let result = parse_solidity_error_message(data);

        assert!(result.contains("Failed to decode"));
    }

    #[test]
    fn test_hexstring_to_vec() {
        let hexstring = "0x68656c6c6f";
        let expected = vec![0x68, 0x65, 0x6c, 0x6c, 0x6f];
        let result = hexstring_to_vec(hexstring).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_hexstring_to_vec_no_prefix() {
        let hexstring = "68656c6c6f";
        let expected = vec![0x68, 0x65, 0x6c, 0x6c, 0x6f];
        let result = hexstring_to_vec(hexstring).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_hexstring_to_vec_invalid_characters() {
        let hexstring = "0x68656c6c6z"; // Invalid character 'z'
        let result = hexstring_to_vec(hexstring);
        assert!(result.is_err());
        if let Err(SimulationError::FatalError(msg)) = result {
            assert!(msg.contains("Invalid hex string"));
        } else {
            panic!("Expected EncodingError");
        }
    }

    #[test]
    fn test_json_deserialize_address_list() {
        let json_input = r#"["0x1234","0xabcd"]"#.as_bytes();
        let result = json_deserialize_address_list(json_input).unwrap();
        assert_eq!(result, vec![vec![0x12, 0x34], vec![0xab, 0xcd]]);
    }

    #[test]
    fn test_json_deserialize_bigint_list() {
        let json_input = r#"["0x0b1a2bc2ec500000","0x02c68af0bb140000"]"#.as_bytes();
        let result = json_deserialize_be_bigint_list(json_input).unwrap();
        assert_eq!(
            result,
            vec![BigInt::from(800000000000000000u64), BigInt::from(200000000000000000u64)]
        );
    }

    #[test]
    fn test_invalid_deserialize_address_list() {
        let json_input = r#"["invalid_hex"]"#.as_bytes();
        let result = json_deserialize_address_list(json_input);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_deserialize_bigint_list() {
        let json_input = r#"["invalid_hex"]"#.as_bytes();
        let result = json_deserialize_be_bigint_list(json_input);
        assert!(result.is_err());
    }
}
