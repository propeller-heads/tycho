use std::collections::HashMap;

use alloy::{
    eips::BlockNumberOrTag,
    hex,
    primitives::{keccak256, Bytes, B256},
    rpc::{
        client::ClientBuilder,
        types::{Block, BlockId},
    },
    sol_types::SolValue,
};
use alloy_rpc_types_trace::geth::{
    GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingCallOptions,
    GethDebugTracingOptions,
};
use serde_json::Value;
use tracing::{error, warn};

use crate::execution::{
    four_byte_client::FourByteClient,
    models::{SimulationInput, SimulationResult},
    traces::print_call_trace,
};

const KNOWN_SIGNATURES: &[&str] =
    &["Error(string)", "Panic(uint256)", "TychoRouter__NegativeSlippage(uint256,uint256)"];

/// Represents a Solidity error signature
///
/// Contains methods to meaningfully decode a revert reason.
#[derive(Debug, Clone)]
pub struct SolidityError {
    // Name of the error, minus the types
    name: String,
    // Hex representation of the keccak hash of the error signature and arguments
    bytes_sig: [u8; 4],
    // Internal types of the error (ex. address, uint)
    types: Vec<String>,
}

impl SolidityError {
    pub fn new(name: String, bytes_sig: [u8; 4], types: Vec<String>) -> Self {
        Self { name, bytes_sig, types }
    }

    pub fn decode(&self, data: &Bytes) -> String {
        if data.len() <= 4 {
            return format!("{}()", self.name);
        }

        // Try to decode the arguments
        match self.decode_error_args(&data[4..]) {
            Ok(args_str) => format!("{}({})", self.name, args_str),
            Err(_) => format!("{}(<decode error>)", self.name),
        }
    }

    /// Decodes the actual arguments from the error using the previously-decoded arg types.
    ///
    /// This ensures that we have as much information regarding the error as possible (for example,
    /// the values of the negative slippage as opposed to just the fact that there was
    /// NegativeSlippage.
    pub fn decode_error_args(&self, data: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
        // This is a simplified version. For full ABI decoding, you'd need alloy-sol-types
        let mut args = Vec::new();
        let mut offset = 0;

        for typ in &self.types {
            match typ.as_str() {
                "string" => {
                    // Decode ABI-encoded string: offset (32 bytes) + length (32 bytes) + data
                    if data.len() >= 64 {
                        // Skip the offset (first 32 bytes) and read length
                        let length =
                            alloy::primitives::U256::from_be_slice(&data[32..64]).to::<usize>();
                        if data.len() >= 64 + length {
                            let string_data = &data[64..64 + length];
                            if let Ok(s) = std::str::from_utf8(string_data) {
                                args.push(format!("\"{}\"", s));
                            } else {
                                // The string data is not UTF-8, it might be nested error bytes
                                // Recursively decode until we get the actual string
                                if let Ok(nested) = Self::decode_nested_error_recursive(string_data)
                                {
                                    args.push(format!("\"{}\"", nested));
                                } else {
                                    args.push(format!("0x{}", hex::encode(string_data)));
                                }
                            }
                        }
                    }
                }
                "uint256" | "uint" => {
                    if data.len() >= offset + 32 {
                        let val =
                            alloy::primitives::U256::from_be_slice(&data[offset..offset + 32]);
                        args.push(val.to_string());
                        offset += 32;
                    }
                }
                "address" => {
                    if data.len() >= offset + 32 {
                        let addr =
                            alloy::primitives::Address::from_slice(&data[offset + 12..offset + 32]);
                        args.push(format!("{:?}", addr));
                        offset += 32;
                    }
                }
                _ => {
                    args.push(format!("0x{}", hex::encode(data)));
                }
            }
        }

        Ok(args.join(", "))
    }

    /// Decodes innermost error of recursive error string.
    ///
    /// Sometimes, another error can be hidden in the string of the highest-level error.
    /// This ensures we get to the bottom of what the true error is, as opposed to returning a
    /// vague "Error" type.
    pub fn decode_nested_error_recursive(
        data: &[u8],
    ) -> Result<String, Box<dyn std::error::Error>> {
        // Check if it starts with Error(string) signature (0x08c379a0)
        if data.len() >= 4 {
            let mut sig = [0u8; 4];
            sig.copy_from_slice(&data[..4]);

            if sig == [0x08, 0xc3, 0x79, 0xa0] {
                // Decode nested Error(string): selector (4 bytes) + offset (32 bytes) + length (32
                // bytes) + string data
                if data.len() >= 68 {
                    let length =
                        alloy::primitives::U256::from_be_slice(&data[36..68]).to::<usize>();
                    if data.len() >= 68 + length {
                        let string_data = &data[68..68 + length];

                        // Try to decode as UTF-8 string
                        if let Ok(s) = std::str::from_utf8(string_data) {
                            return Ok(s.to_string());
                        }

                        // If not UTF-8, it might be another nested error - recurse
                        if let Ok(nested) = Self::decode_nested_error_recursive(string_data) {
                            return Ok(nested);
                        }
                    }
                }
            }
        }

        Err("Failed to decode nested error".into())
    }
}

/// Simulated the execution with meaningful revert reason decoding.
pub struct ExecutionSimulator {
    rpc_url: String,
    // Cache of error signatures that are already known
    error_signatures: HashMap<[u8; 4], SolidityError>,
    // Client used to fetch human-readable signatures from hex signatures
    signature_client: FourByteClient,
}

impl ExecutionSimulator {
    pub fn new(rpc_url: String) -> Self {
        let mut parsed_signatures = HashMap::new();
        for signature in KNOWN_SIGNATURES {
            if let Ok(error) = Self::parse_signature_from_str(signature) {
                parsed_signatures.insert(error.bytes_sig, error);
            }
        }
        Self {
            rpc_url,
            error_signatures: parsed_signatures,
            signature_client: FourByteClient::new(),
        }
    }

    /// Simulate a transaction with trace and return the SimulationResult.
    pub async fn batch_simulate_with_trace(
        &mut self,
        inputs: HashMap<String, SimulationInput>,
        block: &Block,
    ) -> Result<HashMap<String, SimulationResult>, Box<dyn std::error::Error>> {
        // Configure tracing options - use callTracer for better formatted results
        let tracing_options = GethDebugTracingOptions {
            tracer: Some(GethDebugTracerType::BuiltInTracer(
                GethDebugBuiltInTracerType::CallTracer,
            )),
            config: Default::default(),
            tracer_config: Default::default(),
            timeout: None,
        };

        let (simulation_ids, simulation_inputs): (Vec<String>, Vec<SimulationInput>) =
            inputs.into_iter().unzip();

        // Pending (flashblock) blocks have a zero hash — simulate against the pending state
        // so the RPC uses the sequencer's pre-confirmed block rather than a specific number.
        let block_id = if block.header.hash == B256::ZERO {
            BlockId::from(BlockNumberOrTag::Pending)
        } else {
            BlockId::from(block.number())
        };

        let client = ClientBuilder::default().http(self.rpc_url.parse()?);

        let mut batch = client.new_batch();

        let mut futures = Vec::new();
        for input in simulation_inputs.iter() {
            let trace_options = GethDebugTracingCallOptions {
                tracing_options: tracing_options.clone(),
                state_overrides: input.state_overwrites.clone(),
                block_overrides: None,
                tx_index: None,
            };
            let fut =
                batch.add_call("debug_traceCall", &(input.tx.clone(), block_id, trace_options))?;
            futures.push(fut);
        }

        batch.send().await?;

        let mut batch_results = Vec::new();
        for fut in futures {
            batch_results.push(fut.await);
        }

        let mut results = HashMap::new();

        for (i, batch_result) in batch_results.into_iter().enumerate() {
            let simulation_id: &String = &simulation_ids[i];

            match batch_result {
                Ok(trace_response) => {
                    let trace_value: Value = trace_response;

                    let has_error = trace_value
                        .get("error")
                        .and_then(|e| e.as_str())
                        .is_some();

                    if has_error {
                        if tracing::enabled!(tracing::Level::DEBUG) {
                            error!("=== Transaction {} Trace (FAILURE) ===", simulation_id);
                            print_call_trace(&trace_value, 0).await;
                            error!("=== End Trace ===");
                        }

                        // Try to get revert reason from multiple possible locations
                        let reason = if let Some(output) = trace_value
                            .get("output")
                            .and_then(|o| o.as_str())
                        {
                            if output.starts_with("0x") && output.len() > 2 {
                                self.decode_error(output).await?
                            } else {
                                "execution reverted".to_string()
                            }
                        } else if let Some(deepest_reason) =
                            Self::find_deepest_revert_reason(&trace_value)
                        {
                            if deepest_reason.starts_with("0x") {
                                self.decode_error(&deepest_reason)
                                    .await?
                            } else {
                                deepest_reason
                            }
                        } else {
                            "execution reverted".to_string()
                        };

                        results.insert(simulation_id.clone(), SimulationResult::Revert { reason });
                    } else {
                        // Extract return data and gas used from successful trace
                        let return_data = if let Some(output) = trace_value
                            .get("output")
                            .and_then(|o| o.as_str())
                        {
                            hex::decode(output.trim_start_matches("0x"))?
                        } else {
                            Vec::new()
                        };

                        let gas_used = trace_value
                            .get("gasUsed")
                            .and_then(|g| {
                                g.as_u64().or_else(|| {
                                    g.as_str().and_then(|s| {
                                        u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
                                            .ok()
                                    })
                                })
                            })
                            .unwrap_or(0);

                        results.insert(
                            simulation_id.clone(),
                            SimulationResult::Success { return_data, gas_used },
                        );
                    }
                }
                Err(err) => {
                    let reason = format!("Transaction failed: {}", err);
                    if crate::is_block_not_found(&reason) {
                        // All calls share the same block_id in a single batch request to one node,
                        // so if one fails with block-not-found the rest will too. Early-return so
                        // the caller can retry the whole batch.
                        return Err(reason.into());
                    }
                    error!("Error tracing transaction {}: {:?}", simulation_id, err);
                    results.insert(simulation_id.clone(), SimulationResult::Revert { reason });
                }
            }
        }

        Ok(results)
    }

    /// It simulates the transaction and tries to decode the revert reason.
    /// If the revert reason is not known and found in error_signatures, it tries to fetch the
    /// signature using a signature client, such as 4byte.directory. If it still fails, it
    /// returns "UnknownError".
    async fn decode_error(&mut self, data: &str) -> Result<String, Box<dyn std::error::Error>> {
        let data = hex::decode(data.trim_start_matches("0x"))?;
        let data_bytes = Bytes::from(data.clone());

        let result = format!("Revert: 0x{}", hex::encode(&data));

        // Try to match known error signatures
        if data.len() >= 4 {
            let mut sig = [0u8; 4];
            sig.copy_from_slice(&data[..4]);

            if let Some(error) = self.error_signatures.get(&sig) {
                return Ok(error.decode(&data_bytes));
            }
        }

        // Try to decode as string (standard revert reason)
        if data.len() > 4 {
            if let Ok(decoded) = String::abi_decode(&data[4..]) {
                return Ok(decoded);
            }
        }

        // Try fetching using a signature client API
        if data.len() >= 4 {
            let hex_sig = format!("0x{}", hex::encode(&data[..4]));

            match self
                .signature_client
                .get_signature(&hex_sig)
                .await
            {
                Ok(fetched_sig) => {
                    if let Ok(error) = Self::parse_signature_from_str(&fetched_sig) {
                        let decoded = error.decode(&data_bytes);
                        self.error_signatures
                            .insert(error.bytes_sig, error);
                        return Ok(decoded);
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch signature from 4bytes: {}", e);
                    return Ok(hex_sig);
                }
            }
        }

        Ok(result)
    }

    /// Parses a signature from a string into a SolidityError
    ///
    /// Extracts the name of the error, its internal types, and the hex representation of the keccak
    /// hash of the error and its types.
    fn parse_signature_from_str(
        signature: &str,
    ) -> Result<SolidityError, Box<dyn std::error::Error>> {
        if signature.contains(' ') {
            return Err(format!("Found whitespace in signature: {}", signature).into());
        }

        // Parse signature like "Error(string)" or "Panic(uint256)"
        let re = regex::Regex::new(r"(.*?)\((.*?)\)").unwrap();
        let captures = re
            .captures(signature)
            .ok_or("Failed to parse signature")?;

        let name = captures
            .get(1)
            .unwrap()
            .as_str()
            .to_string();
        let types_str = captures.get(2).unwrap().as_str();

        let types: Vec<String> = if types_str.is_empty() {
            Vec::new()
        } else {
            types_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        };

        // Hash the signature so we know immediately if we see this hash again
        let byte_sig = keccak256(signature.as_bytes());
        let mut sig_array = [0u8; 4];
        sig_array.copy_from_slice(&byte_sig[..4]);

        Ok(SolidityError::new(name, sig_array, types))
    }

    /// Decode a Panic(uint256) error code to a human-readable message
    pub fn decode_panic_code(code: u64) -> String {
        match code {
            0x01 => "assertion failed".to_string(),
            0x11 => "arithmetic underflow or overflow".to_string(),
            0x12 => "division or modulo by zero".to_string(),
            0x21 => "invalid enum value".to_string(),
            0x22 => "storage byte array incorrectly encoded".to_string(),
            0x31 => "pop on empty array".to_string(),
            0x32 => "array index out of bounds".to_string(),
            0x41 => "out of memory".to_string(),
            0x51 => "invalid internal function".to_string(),
            _ => format!("panic code 0x{:x}", code),
        }
    }

    /// Recursively find the deepest revert reason in a trace
    ///
    /// This is used because sometimes a meaningful error may be under "execution reverted" if we
    /// only take the highest-level error.
    fn find_deepest_revert_reason(trace: &Value) -> Option<String> {
        // Check for output field that might contain revert data
        if let Some(output) = trace
            .get("output")
            .and_then(|o| o.as_str())
        {
            if !output.is_empty() && output != "0x" {
                // Try to decode using the recursive decoder for nested errors
                if let Ok(data) = hex::decode(output.trim_start_matches("0x")) {
                    if data.len() >= 4 {
                        // Check if it's Error(string) selector (0x08c379a0)
                        if data[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
                            // Use the recursive decoder to handle nested Error(string) wrapping
                            if let Ok(reason) = SolidityError::decode_nested_error_recursive(&data)
                            {
                                return Some(reason);
                            }
                        }
                        // Check if it's Panic(uint256) selector (0x4e487b71)
                        else if data[0..4] == [0x4e, 0x48, 0x7b, 0x71] && data.len() >= 36 {
                            // Decode the panic code (uint256)
                            let panic_code =
                                alloy::primitives::U256::from_be_slice(&data[4..36]).to::<u64>();
                            return Some(Self::decode_panic_code(panic_code));
                        }
                    }
                }
                // Return raw output if we can't decode it
                return Some(output.to_string());
            }
        }

        // Check for revertReason field
        if let Some(revert_reason) = trace
            .get("revertReason")
            .and_then(|r| r.as_str())
        {
            if !revert_reason.is_empty() {
                // The revertReason might contain raw bytes with error encoding
                let bytes = revert_reason.as_bytes();
                if bytes.len() >= 4 {
                    // Check if it's Error(string) selector (0x08c379a0)
                    if bytes[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
                        if let Ok(decoded) = SolidityError::decode_nested_error_recursive(bytes) {
                            return Some(decoded);
                        }
                    }
                    // Check if it's Panic(uint256) selector (0x4e487b71)
                    else if bytes[0..4] == [0x4e, 0x48, 0x7b, 0x71] && bytes.len() >= 36 {
                        let panic_code =
                            alloy::primitives::U256::from_be_slice(&bytes[4..36]).to::<u64>();
                        return Some(Self::decode_panic_code(panic_code));
                    }
                }
                return Some(revert_reason.to_string());
            }
        }

        // Recursively check nested calls
        if let Some(calls) = trace
            .get("calls")
            .and_then(|c| c.as_array())
        {
            for call in calls {
                if let Some(reason) = Self::find_deepest_revert_reason(call) {
                    return Some(reason);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_signature() {
        let sig = "Error(string)";
        let error = ExecutionSimulator::parse_signature_from_str(sig).unwrap();
        assert_eq!(error.name, "Error");
        assert_eq!(error.types, vec!["string"]);
    }

    #[test]
    fn test_parse_signature_no_args() {
        let sig = "EmptyError()";
        let error = ExecutionSimulator::parse_signature_from_str(sig).unwrap();
        assert_eq!(error.name, "EmptyError");
        assert!(error.types.is_empty());
    }

    #[test]
    fn test_parse_signature_multiple_args() {
        let sig = "ComplexError(uint256,address,string)";
        let error = ExecutionSimulator::parse_signature_from_str(sig).unwrap();
        assert_eq!(error.name, "ComplexError");
        assert_eq!(error.types, vec!["uint256", "address", "string"]);
    }

    #[tokio::test]
    #[ignore = "requires internet connection"]
    async fn test_match_error_with_fourbyte_lookup() {
        // Create a fetcher with no pre-loaded signatures
        let mut fetcher = ExecutionSimulator::new("http://dummy".to_string());

        // Test data with signature 0x90bfb865 (WrappedError)
        let error_data = "0x90bfb865";

        let result = fetcher.decode_error(error_data).await;
        assert!(result.is_ok());

        let decoded = result.unwrap();
        // Should contain WrappedError after 4byte lookup
        assert!(decoded.contains("WrappedError"));
    }
}
