use alloy::{
    hex,
    primitives::{map::AddressHashMap, Address, B256, U256},
    rpc::types::{state::AccountOverride, Block},
};

use crate::execution::models::Transaction;

/// Metadata about storage slot overwrites to enable human-readable logging
#[derive(Debug, Clone, Default)]
pub struct OverwriteMetadata {
    /// Mapping of storage addresses to their slot metadata
    pub slots: AddressHashMap<Vec<SlotMetadata>>,
}

/// Metadata for a single storage slot
#[derive(Debug, Clone)]
pub enum SlotMetadata {
    /// Balance slot: balance\[owner\]
    Balance { owner: Address, slot: B256 },
    /// Allowance slot: allowance\[owner\]\[spender\]
    Allowance { owner: Address, spender: Address, slot: B256 },
}

impl OverwriteMetadata {
    pub fn new() -> Self {
        Self { slots: AddressHashMap::default() }
    }

    pub fn add_balance(&mut self, storage_address: Address, owner: Address, slot: B256) {
        self.slots
            .entry(storage_address)
            .or_default()
            .push(SlotMetadata::Balance { owner, slot });
    }

    pub fn add_allowance(
        &mut self,
        storage_address: Address,
        owner: Address,
        spender: Address,
        slot: B256,
    ) {
        self.slots
            .entry(storage_address)
            .or_default()
            .push(SlotMetadata::Allowance { owner, spender, slot });
    }
}

/// Parameters for Tenderly simulation
#[derive(Debug, Clone, Default)]
pub struct TenderlySimParams {
    pub from: Option<String>,
    pub contract_address: Option<String>,
    pub value: Option<String>,
    pub raw_function_input: Option<String>,
    pub network: Option<String>,
    pub block: Option<String>,
    pub block_index: Option<String>,
    pub header_block_number: Option<String>,
    pub gas: Option<String>,
    pub gas_price: Option<String>,
}

/// Build a Tenderly simulator URL with the given parameters
///
/// Generates a URL pointing to the Tenderly simulator configured with the provided parameters.
/// Parameters can be provided directly via `overrides` or extracted from the `tx` and `block`
/// arguments.
///
/// # Arguments
///
/// * `overrides` - Simulation parameters to override defaults with
/// * `tx` - Transaction object from which data can be extracted if not provided in `overrides`
/// * `block` - Block object from which data can be extracted if not provided in `overrides`
/// * `caller` - The address calling the transaction
/// * `state_overrides` - State overrides for balance and allowance slots
///
/// # Returns
///
/// A string representing the URL pointing to the Tenderly simulator with the defined parameters
pub fn build_tenderly_url(
    overrides: &TenderlySimParams,
    tx: Option<&Transaction>,
    block: Option<&Block>,
    caller: Address,
) -> String {
    // Extract transaction data
    let (tx_to, tx_value, tx_data) = if let Some(transaction) = tx {
        let to_addr = format!("0x{}", hex::encode(transaction.to()));
        let value = transaction.value().to_string();
        let data = format!("0x{}", hex::encode(transaction.data()));
        (Some(to_addr), Some(value), Some(data))
    } else {
        (None, None, None)
    };

    // Extract block number
    let block_number = block.map(|b| (b.header.number + 1).to_string());

    // Build parameters with overrides taking priority
    let from = overrides
        .from
        .clone()
        .unwrap_or_else(|| format!("0x{:x}", caller));

    let contract_address = overrides
        .contract_address
        .clone()
        .or(tx_to)
        .unwrap_or_default();

    let value = overrides
        .value
        .clone()
        .or(tx_value)
        .unwrap_or_else(|| "0".to_string());

    let raw_function_input = overrides
        .raw_function_input
        .clone()
        .or(tx_data)
        .unwrap_or_default();

    // Default to Ethereum mainnet (chain ID 1)
    let network = overrides
        .network
        .clone()
        .unwrap_or_else(|| "1".to_string());

    let block_param = overrides
        .block
        .clone()
        .or(block_number)
        .unwrap_or_default();

    // Build query parameters
    let mut params = vec![
        ("from", from),
        ("contractAddress", contract_address),
        ("value", value),
        ("rawFunctionInput", raw_function_input),
        ("network", network),
        ("block", block_param),
    ];

    // Add optional parameters if present
    if let Some(block_index) = &overrides.block_index {
        params.push(("blockIndex", block_index.clone()));
    }
    if let Some(header_block_number) = &overrides.header_block_number {
        params.push(("headerBlockNumber", header_block_number.clone()));
    }
    if let Some(gas) = &overrides.gas {
        params.push(("gas", gas.clone()));
    }
    if let Some(gas_price) = &overrides.gas_price {
        params.push(("gasPrice", gas_price.clone()));
    }

    // URL encode parameters
    let query_string = params
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("https://dashboard.tenderly.co/tvinagre/project/simulator/new?{}", query_string)
}

pub fn get_overwrites_string(
    overwrites: &AddressHashMap<AccountOverride>,
    metadata: &Option<OverwriteMetadata>,
) -> String {
    let mut tokens = Vec::new();

    for (address, account_override) in overwrites {
        let mut token_overwrites = Vec::new();

        // Add balance if present
        if let Some(balance) = &account_override.balance {
            token_overwrites.push(format!("native_balance: {balance}"));
        }

        // Add storage slots if present
        if let Some(state_diff) = &account_override.state_diff {
            for (slot, value) in state_diff {
                // First add the raw storage slot info
                token_overwrites.push(format!("slot: 0x{slot:x}"));

                // If metadata is available, add human-readable format
                if let Some(meta) = metadata {
                    if let Some(slot_metas) = meta.slots.get(address) {
                        for slot_meta in slot_metas {
                            match slot_meta {
                                SlotMetadata::Balance { owner, slot: meta_slot }
                                    if meta_slot == slot =>
                                {
                                    let amount = U256::from_be_slice(value.as_slice());
                                    token_overwrites
                                        .push(format!("balance[0x{:x}]: {}", owner, amount));
                                }
                                SlotMetadata::Allowance { owner, spender, slot: meta_slot }
                                    if meta_slot == slot =>
                                {
                                    let amount = U256::from_be_slice(value.as_slice());
                                    token_overwrites.push(format!(
                                        "allowance[0x{:x}][0x{:x}]: {}",
                                        owner, spender, amount
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        if !token_overwrites.is_empty() {
            tokens.push(format!("0x{:x}: [{}]", address, token_overwrites.join(", ")));
        }
    }

    format!("{{{}}}", tokens.join(", "))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use num_bigint::BigUint;

    use super::*;

    #[test]
    fn test_build_url_with_transaction() {
        let tx = Transaction::new(
            vec![0xde, 0xad, 0xbe, 0xef].into(),
            BigUint::from(1000u32),
            vec![0x12, 0x34, 0x56, 0x78],
            BigUint::ZERO,
        );

        let caller = address!("f847a638E44186F3287ee9F8cAF73FF4d4B80784");
        let overrides = TenderlySimParams::default();

        let url = build_tenderly_url(&overrides, Some(&tx), None, caller);

        assert!(url.contains("from=0xf847a638e44186f3287ee9f8caf73ff4d4b80784"));
        assert!(url.contains("contractAddress=0xdeadbeef"));
        assert!(url.contains("value=1000"));
        assert!(url.contains("rawFunctionInput=0x12345678"));
    }

    #[test]
    fn test_build_url_with_overrides() {
        let caller = address!("0000000000000000000000000000000000000000");
        let overrides = TenderlySimParams {
            from: Some("0x1234567890123456789012345678901234567890".to_string()),
            contract_address: Some("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string()),
            value: Some("500".to_string()),
            network: Some("5".to_string()),
            ..Default::default()
        };

        let url = build_tenderly_url(&overrides, None, None, caller);

        assert!(url.contains("from=0x1234567890123456789012345678901234567890"));
        assert!(url.contains("contractAddress=0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"));
        assert!(url.contains("value=500"));
        assert!(url.contains("network=5"));
    }

    #[test]
    fn test_build_url_with_block() {
        let caller = address!("0000000000000000000000000000000000000000");
        let overrides =
            TenderlySimParams { block: Some("12345678".to_string()), ..Default::default() };

        let url = build_tenderly_url(&overrides, None, None, caller);

        assert!(url.contains("block=12345678"));
    }
}
