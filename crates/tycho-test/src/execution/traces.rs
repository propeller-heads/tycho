//! Transaction trace analysis with foundry signature decoding.
//!
//! This module provides utilities for analyzing Ethereum transaction traces
//! and decoding method signatures using foundry's comprehensive signature database.

use alloy::{
    dyn_abi::{DynSolType, DynSolValue},
    hex,
};
use colored::Colorize;
use serde_json::Value;
use tycho_simulation::evm::traces::SignaturesIdentifier;

use crate::execution::execution_simulator::{ExecutionSimulator, SolidityError};

/// Decode method selectors and return function info
pub async fn decode_method_selector_with_info(input: &str) -> Option<(String, Vec<DynSolType>)> {
    if input.len() < 10 || !input.starts_with("0x") {
        return None;
    }

    let selector_bytes = hex::decode(&input[2..10]).ok()?;
    if selector_bytes.len() != 4 {
        return None;
    }
    let selector: [u8; 4] = selector_bytes.try_into().ok()?;
    let selector_fixed: alloy::primitives::FixedBytes<4> = selector.into();

    // Use foundry's signature identifier
    if let Ok(sig_identifier) = SignaturesIdentifier::new(true) {
        if let Some(signature) = sig_identifier
            .identify_function(selector_fixed)
            .await
        {
            let formatted_sig = format!(
                "{}({})",
                signature.name,
                signature
                    .inputs
                    .iter()
                    .map(|p| p.ty.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );

            // Filter out scam/honeypot signatures
            if is_legitimate_signature(&formatted_sig) {
                // Parse parameter types
                let param_types: Vec<DynSolType> = signature
                    .inputs
                    .iter()
                    .filter_map(|p| p.ty.as_str().parse().ok())
                    .collect();

                return Some((signature.name, param_types));
            }
        }
    }

    None
}

/// Decode method selectors using foundry's signature database with scam filtering
pub async fn decode_method_selector(input: &str) -> Option<String> {
    if let Some((name, param_types)) = decode_method_selector_with_info(input).await {
        let type_names: Vec<String> = param_types
            .iter()
            .map(|t| t.to_string())
            .collect();
        return Some(format!("{}({})", name, type_names.join(",")));
    }

    // Return unknown if not found - print entire input
    if input.len() >= 10 {
        Some(format!("{} (unknown)", input))
    } else {
        None
    }
}

/// Decode function calldata and format with parameter values
pub async fn decode_function_with_params(input: &str) -> Option<String> {
    if input.len() < 10 || !input.starts_with("0x") {
        return decode_method_selector(input).await;
    }

    if let Some((name, param_types)) = decode_method_selector_with_info(input).await {
        // Try to decode the calldata
        if input.len() > 10 {
            let calldata_hex = &input[10..]; // Skip the 4-byte selector
            if let Ok(calldata) = hex::decode(calldata_hex) {
                if let Ok(DynSolValue::Tuple(values)) =
                    DynSolType::Tuple(param_types.clone()).abi_decode(&calldata)
                {
                    let formatted_params: Vec<String> = values
                        .iter()
                        .zip(param_types.iter())
                        .map(|(value, ty)| format_parameter_value(value, ty))
                        .collect();

                    return Some(format!("{}({})", name, formatted_params.join(", ")));
                }
            }
        }

        // Fallback: if decoding fails, put the whole calldata inside the method call
        return Some(format!("{}({})", name, input));
    }

    // Return unknown if not found - print entire input
    Some(format!("{} (unknown)", input))
}

/// Format a parameter value for display
fn format_parameter_value(value: &DynSolValue, _ty: &DynSolType) -> String {
    match value {
        DynSolValue::Address(addr) => format!("{:#x}", addr),
        DynSolValue::Uint(uint, _) => {
            let value_str = uint.to_string();
            // Add scientific notation for large numbers
            if value_str.len() > 15 {
                format!("{} [{}e{}]", value_str, &value_str[0..4], value_str.len() - 1)
            } else {
                value_str
            }
        }
        DynSolValue::Int(int, _) => int.to_string(),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::Bytes(bytes) => format!("0x{}", hex::encode(bytes)),
        DynSolValue::FixedBytes(bytes, _) => format!("0x{}", hex::encode(bytes)),
        DynSolValue::String(s) => format!("\"{}\"", s),
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            let elements: Vec<String> = arr
                .iter()
                .map(|v| format_parameter_value(v, _ty))
                .collect();
            format!("[{}]", elements.join(", "))
        }
        DynSolValue::Tuple(tuple) => {
            let elements: Vec<String> = tuple
                .iter()
                .map(|v| format_parameter_value(v, _ty))
                .collect();
            format!("({})", elements.join(", "))
        }
        DynSolValue::Function(_) => "function".to_string(),
        DynSolValue::CustomStruct { .. } => "struct".to_string(),
    }
}

/// Check if a signature looks legitimate (not a scam/honeypot)
fn is_legitimate_signature(signature: &str) -> bool {
    let sig_lower = signature.to_lowercase();

    // Reject obvious scam patterns
    let scam_patterns = [
        "watch_tg",
        "_tg_",
        "telegram",
        "discord",
        "twitter",
        "social",
        "invite",
        "gift",
        "bonus",
        "airdrop",
        "referral",
        "ref_",
        "_reward",
        "claim_reward",
        "_bonus",
        "_gift",
        "_invite",
        "honeypot",
        "rug",
        "scam",
        "phish",
        "sub2juniononyoutube",
        "youtube",
        "sub2",
        "junion",
    ];

    for pattern in &scam_patterns {
        if sig_lower.contains(pattern) {
            return false;
        }
    }

    // Reject signatures that are suspiciously long (likely auto-generated scam functions)
    if signature.len() > 80 {
        return false;
    }

    // Reject signatures with too many underscores (common in scam functions)
    let underscore_count = signature.matches('_').count();
    if underscore_count > 3 {
        return false;
    }

    // Reject signatures that look like random hex or encoded data
    if signature
        .matches(char::is_numeric)
        .count() >
        signature.len() / 2
    {
        return false;
    }

    true
}

/// Trace printing with foundry-style formatting and colors
pub async fn print_call_trace(call: &Value, depth: usize) {
    if depth == 0 {
        eprintln!("{}", "Traces:".cyan().bold());
    }

    if let Some(call_obj) = call.as_object() {
        // Parse trace data
        let call_type = call_obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN");

        let _from = call_obj
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("0x?");

        let to = call_obj
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or("0x?");

        let gas_used = call_obj
            .get("gasUsed")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0");

        let _value = call_obj
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0");

        // Convert hex values for display
        let gas_used_dec = if let Some(stripped) = gas_used.strip_prefix("0x") {
            u64::from_str_radix(stripped, 16).unwrap_or(0)
        } else {
            gas_used.parse().unwrap_or(0)
        };

        // Check if call failed
        let has_error = call_obj.get("error").is_some();
        let has_other_error = ["revert", "reverted", "message", "errorMessage", "reason"]
            .iter()
            .any(|field| call_obj.get(*field).is_some());
        let call_failed = has_error || has_other_error;

        // Create tree structure prefix
        let tree_prefix = if depth == 0 { "".to_string() } else { "  ".repeat(depth) + "├─ " };

        // Get input for method signature decoding
        let input = call_obj
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("0x");

        // Decode method signature with parameters
        let method_sig = if !input.is_empty() && input != "0x" {
            decode_function_with_params(input)
                .await
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            format!("{}()", call_type.to_lowercase())
        };

        // Format the main call line with colors
        let gas_str = format!("[{}]", gas_used_dec);
        let call_part = format!("{}::{}", to, method_sig);

        if call_failed {
            println!("{}{} {}", tree_prefix, gas_str, call_part.red());
        } else {
            println!("{}{} {}", tree_prefix, gas_str, call_part.green());
        }

        // Print return/revert information with proper indentation
        let result_indent = "  ".repeat(depth + 1) + "└─ ← ";

        // Check for various error/revert patterns
        let mut found_error = false;

        if let Some(error) = call_obj.get("error") {
            println!("{result_indent} [Error] {error}");
            found_error = true;
        }

        // Check for other possible error fields
        for error_field in ["revert", "reverted", "message", "errorMessage", "reason"] {
            if let Some(error_val) = call_obj.get(error_field) {
                println!("{}[{}] {}", result_indent, error_field, error_val);
                found_error = true;
            }
        }

        // Check for revert data in output (sometimes revert reasons are hex-encoded in output)
        if let Some(output) = call_obj
            .get("output")
            .and_then(|v| v.as_str())
        {
            if !output.is_empty() && output != "0x" {
                // Try to decode revert reason from output if it looks like revert data
                if let Ok(data) = hex::decode(output.trim_start_matches("0x")) {
                    if data.len() >= 4 {
                        // Check if it's Error(string) selector (0x08c379a0)
                        if data[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
                            // Use the recursive decoder to handle nested Error(string) wrapping
                            if let Ok(reason) = SolidityError::decode_nested_error_recursive(&data)
                            {
                                eprintln!(
                                    "{}{}",
                                    result_indent,
                                    format!("[Revert] {}", reason).red()
                                );
                                found_error = true;
                            }
                        }
                        // Check if it's Panic(uint256) selector (0x4e487b71)
                        else if data[0..4] == [0x4e, 0x48, 0x7b, 0x71] && data.len() >= 36 {
                            // Decode the panic code (uint256)
                            let panic_code =
                                alloy::primitives::U256::from_be_slice(&data[4..36]).to::<u64>();
                            let panic_msg = ExecutionSimulator::decode_panic_code(panic_code);
                            println!("{}{}", result_indent, format!("[Panic] {}", panic_msg).red());
                            found_error = true;
                        }
                    }
                }

                if !found_error {
                    println!("{}[Return] {}", result_indent, output);
                }
            } else if !found_error {
                println!("{}[Return]", result_indent);
            }
        }

        // If we haven't found any output yet and there was no explicit error, show empty return
        if !found_error && call_obj.get("output").is_none() {
            println!("{}{}", result_indent, "[Return]".green());
        }

        // Recursively print nested calls
        if let Some(calls) = call_obj.get("calls") {
            if let Some(calls_array) = calls.as_array() {
                for nested_call in calls_array {
                    Box::pin(print_call_trace(nested_call, depth + 1)).await;
                }
            }
        }
    }
}
