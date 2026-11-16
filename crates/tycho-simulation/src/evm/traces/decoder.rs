use std::collections::HashMap;

use alloy::{
    dyn_abi::{DynSolValue, EventExt, FunctionExt, JsonAbiExt},
    json_abi::{Event, Function},
    primitives::{Address, LogData, Selector, B256},
};
use revm_inspectors::tracing::{
    types::{CallTrace, DecodedCallData, DecodedCallLog, DecodedCallTrace},
    CallTraceArena,
};

use crate::evm::traces::{
    config::TraceConfig,
    etherscan::EtherscanIdentifier,
    signatures::{SelectorKind, SignaturesIdentifier},
};

/// Format DynSolValue without verbose type information
fn format_dyn_sol_value(value: &DynSolValue) -> String {
    match value {
        DynSolValue::Address(addr) => addr.to_string(),
        DynSolValue::Bytes(bytes) => {
            if bytes.is_empty() {
                "[]".to_string()
            } else {
                format!("0x{}", hex::encode(bytes))
            }
        }
        DynSolValue::FixedBytes(bytes, _) => format!("0x{}", hex::encode(bytes)),
        DynSolValue::Uint(val, _) => val.to_string(),
        DynSolValue::Int(val, _) => val.to_string(),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::String(s) => format!("\"{}\"", s),
        DynSolValue::Array(arr) => {
            let formatted_items: Vec<String> = arr
                .iter()
                .map(format_dyn_sol_value)
                .collect();
            format!("[{}]", formatted_items.join(", "))
        }
        DynSolValue::FixedArray(arr) => {
            let formatted_items: Vec<String> = arr
                .iter()
                .map(format_dyn_sol_value)
                .collect();
            format!("[{}]", formatted_items.join(", "))
        }
        DynSolValue::Tuple(tuple) => {
            let formatted_items: Vec<String> = tuple
                .iter()
                .map(format_dyn_sol_value)
                .collect();
            format!("({})", formatted_items.join(", "))
        }
        DynSolValue::Function(func) => format!("function({})", hex::encode(func.as_slice())),
        DynSolValue::CustomStruct { name, tuple, .. } => {
            let formatted_items: Vec<String> = tuple
                .iter()
                .map(format_dyn_sol_value)
                .collect();
            format!("{}({})", name, formatted_items.join(", "))
        }
    }
}

/// Simplified trace decoder that doesn't depend on foundry git repos
pub struct CallTraceDecoder {
    /// Known contract labels
    labels: HashMap<Address, String>,
    /// Known contract names
    contracts: HashMap<Address, String>,
    /// Known function signatures
    functions: HashMap<Selector, Function>,
    /// Known event signatures (topic hash -> event)
    events: HashMap<B256, Event>,
    /// Etherscan identifier (optional)
    etherscan_identifier: Option<EtherscanIdentifier>,
    /// Signature identifier for unknown functions/events
    signature_identifier: Option<SignaturesIdentifier>,
}

impl CallTraceDecoder {
    /// Create a new trace decoder
    pub fn new() -> Self {
        Self {
            labels: HashMap::new(),
            contracts: HashMap::new(),
            functions: HashMap::new(),
            events: HashMap::new(),
            etherscan_identifier: None,
            signature_identifier: None,
        }
    }

    /// Create a decoder with configuration
    pub async fn with_config(
        config: &TraceConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut decoder = Self::new();

        // Add custom labels from config
        decoder
            .labels
            .extend(config.labels.clone());

        // Set up Etherscan identifier if available
        if let Ok(Some(identifier)) = EtherscanIdentifier::new(config) {
            decoder.etherscan_identifier = Some(identifier);
        }

        // Set up signature identifier
        decoder.signature_identifier = Some(SignaturesIdentifier::new(config.offline)?);

        Ok(decoder)
    }

    /// Identify unknown addresses in the trace
    pub async fn identify_trace(
        &mut self,
        arena: &CallTraceArena,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Collect unknown addresses
        let unknown_addresses: Vec<Address> = arena
            .nodes()
            .iter()
            .map(|node| node.trace.address)
            .filter(|addr| !self.labels.contains_key(addr) && !self.contracts.contains_key(addr))
            .collect();

        if unknown_addresses.is_empty() {
            return Ok(());
        }

        // Try Etherscan identification
        if let Some(identifier) = &mut self.etherscan_identifier {
            let identified = identifier
                .identify_addresses(&unknown_addresses)
                .await;
            for addr_info in identified {
                if let Some(label) = addr_info.label {
                    self.labels
                        .insert(addr_info.address, label);
                }
                if let Some(contract) = addr_info.contract {
                    self.contracts
                        .insert(addr_info.address, contract);
                }
                if let Some(abi) = addr_info.abi {
                    // Add functions and events from the ABI
                    for function in abi.functions() {
                        self.functions
                            .insert(function.selector(), function.clone());
                    }
                    for event in abi.events() {
                        self.events
                            .insert(event.selector(), event.clone());
                    }
                }
            }
        }

        // Collect unknown selectors for signature identification
        if let Some(sig_identifier) = &mut self.signature_identifier {
            let mut selectors = Vec::new();

            // Collect function selectors
            for node in arena.nodes() {
                if node.trace.data.len() >= 4 {
                    let selector = Selector::from_slice(&node.trace.data[..4]);
                    if !self.functions.contains_key(&selector) {
                        selectors.push(SelectorKind::Function(selector));
                    }
                }

                // Collect event signatures from logs
                for log in &node.logs {
                    if !log.raw_log.topics().is_empty() {
                        let topic = log.raw_log.topics()[0];
                        if !self.events.contains_key(&topic) {
                            selectors.push(SelectorKind::Event(topic));
                        }
                    }
                }
            }

            // Identify signatures
            sig_identifier
                .identify_batch(&selectors)
                .await?;
        }

        Ok(())
    }

    /// Decode the entire trace arena
    pub async fn decode_arena(
        &mut self,
        arena: &mut CallTraceArena,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // First identify unknown addresses and signatures
        self.identify_trace(arena).await?;

        // Then decode each node
        for node in arena.nodes_mut() {
            node.trace.decoded = Some(Box::new(
                self.decode_call_trace(&node.trace)
                    .await,
            ));

            // Decode logs
            for log in &mut node.logs {
                log.decoded = Some(Box::new(self.decode_log(&log.raw_log).await));
            }
        }

        Ok(())
    }

    /// Decode a single call trace
    pub async fn decode_call_trace(&mut self, trace: &CallTrace) -> DecodedCallTrace {
        let label = self.labels.get(&trace.address).cloned();

        // Handle contract creation
        if trace.kind.is_any_create() {
            return DecodedCallTrace {
                label,
                call_data: Some(DecodedCallData {
                    signature: "constructor()".to_string(),
                    args: vec![],
                }),
                return_data: None,
            };
        }

        // Skip if no calldata
        if trace.data.len() < 4 {
            return DecodedCallTrace { label, call_data: None, return_data: None };
        }

        let selector = Selector::from_slice(&trace.data[..4]);

        // Try to find known function
        let function = if let Some(func) = self.functions.get(&selector) {
            Some(func.clone())
        } else if let Some(sig_identifier) = &mut self.signature_identifier {
            sig_identifier
                .identify_function(selector)
                .await
        } else {
            None
        };

        let call_data = if let Some(func) = &function {
            // Try to decode function input
            let args = if trace.data.len() > 4 {
                func.abi_decode_input(&trace.data[4..])
                    .map(|values| {
                        values
                            .iter()
                            .map(format_dyn_sol_value)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            };

            Some(DecodedCallData { signature: func.signature(), args })
        } else {
            // Unknown function
            Some(DecodedCallData {
                signature: format!("0x{}", hex::encode(&trace.data[..4])),
                args: if trace.data.len() > 4 {
                    vec![hex::encode(&trace.data[4..])]
                } else {
                    vec![]
                },
            })
        };

        // Decode return data if function succeeded
        let return_data = if trace.success && !trace.output.is_empty() {
            if let Some(func) = &function {
                func.abi_decode_output(&trace.output)
                    .map(|values| {
                        values
                            .iter()
                            .map(format_dyn_sol_value)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .ok()
            } else {
                Some(hex::encode(&trace.output))
            }
        } else if !trace.success && !trace.output.is_empty() {
            // Try to decode revert reason
            if trace.output.len() >= 68 && trace.output[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
                // Standard revert string - just show hex for now
                Some(format!("Error: 0x{}", hex::encode(&trace.output)))
            } else {
                Some(hex::encode(&trace.output))
            }
        } else {
            None
        };

        DecodedCallTrace { label, call_data, return_data }
    }

    /// Decode a log entry
    pub async fn decode_log(&mut self, log: &LogData) -> DecodedCallLog {
        if log.topics().is_empty() {
            return DecodedCallLog { name: None, params: None };
        }

        let topic = log.topics()[0];

        // Try to find known event
        let event = if let Some(evt) = self.events.get(&topic) {
            Some(evt.clone())
        } else if let Some(sig_identifier) = &mut self.signature_identifier {
            sig_identifier
                .identify_event(topic)
                .await
        } else {
            None
        };

        if let Some(event) = &event {
            // Try to decode the log
            if let Ok(decoded) = event.decode_log(log) {
                let params: Vec<(String, String)> = decoded
                    .indexed
                    .iter()
                    .chain(decoded.body.iter())
                    .zip(event.inputs.iter())
                    .map(|(value, input)| (input.name.clone(), format_dyn_sol_value(value)))
                    .collect();

                return DecodedCallLog { name: Some(event.name.clone()), params: Some(params) };
            }
        }

        // Unknown event - just show the raw data
        DecodedCallLog {
            name: Some(format!("0x{}", hex::encode(topic.as_slice()))),
            params: Some(vec![("data".to_string(), hex::encode(&log.data))]),
        }
    }
}
