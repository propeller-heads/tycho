use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{keccak256, map::AddressHashMap, Address, FixedBytes, Keccak256, B256, U256},
    providers::Provider,
    rpc::types::{state::AccountOverride, Block, BlockId, TransactionRequest},
    sol_types::SolValue,
};
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use tracing::debug;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    traits::{AllowanceSlotDetector, BalanceSlotDetector},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry, ROUTER_ETH_ADDRESS,
    },
    models::{ClientFeeParams, EncodedSolution, Solution, Swap},
};
use tycho_simulation::{
    evm::protocol::u256_num::biguint_to_u256, protocol::models::ProtocolComponent,
};

use crate::{
    execution::{
        models::{RouterOverwritesData, Transaction},
        tenderly::OverwriteMetadata,
    },
    rpc_tools::RPCTools,
};

const USER_ADDR: &str = "0xf847a638E44186F3287ee9F8cAF73FF4d4B80784";
const GAS_LIMIT: u64 = 100_000_000;
// 1_000 native tokens (10^21 wei): covers 100M gas at up to ~10_000 gwei
const GAS_RESERVE: U256 = alloy::uint!(1_000_000_000_000_000_000_000_U256);
pub const EXECUTOR_ADDRESS: &str = "0xaE04CA7E9Ed79cBD988f6c536CE11C621166f41B";
// Fixed address used to plant FeeCalculator bytecode in state overrides.
pub const FEE_CALCULATOR_ADDRESS: &str = "0xfEEcA1C0fEEcA1C0fEEcA1C0fEEcA1C0fEEcA1C0";
const FERMISWAP_REGISTRY_ADDRESS: &str = "0xDA7AFeEd01fe625cF15D187A19F94B45F00b8C5f";
// The Fermi engine currently pointed at by the swapper's storage slot 2. Fermi migrates engines
// by re-pointing the swapper (last: 2026-07-21, block 25581704); update this together with
// `engine_address` in the fermiswap substreams params, or lane overwrites patch a dead slot.
const FERMISWAP_TARGET_ADDRESS: &str = "0x90f73fEA1Ee2Dc514d4dbAc0bfF7ff04b933767f";
// BopAMM prices its books from the same PrioUpdateRegistry as FermiSwap; only the registry
// `target` differs — it is the pricing module, and the lane index is the book's `assetId`.
const BOPAMM_REGISTRY_ADDRESS: &str = "0xDA7AFeEd01fe625cF15D187A19F94B45F00b8C5f";
const BOPAMM_MODULE_ADDRESS: &str = "0xbc60639345dfa607d73b74e88c2d54d8b8ad7cc3";

/// Contains the detected storage slots for a token.
#[derive(Debug, Clone, Default)]
pub struct TokenSlots {
    pub balance_storage_addr: Vec<u8>,
    pub balance_slot: Vec<u8>,
    pub allowance_storage_addr: Vec<u8>,
    pub allowance_slot: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
pub fn encode_swap(
    component: &ProtocolComponent,
    state: Option<Arc<dyn ProtocolSim>>,
    sell_token: &Token,
    buy_token: &Token,
    amount_in: BigUint,
    chain: Chain,
    executors_json: Option<String>,
    gas_usage: BigUint,
    expected_amount_out: BigUint,
) -> miette::Result<(Solution, Transaction)> {
    let solution = create_solution(
        component.clone(),
        state,
        sell_token.clone(),
        buy_token.clone(),
        amount_in.clone(),
        gas_usage.clone(),
        expected_amount_out,
    )?;
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(executors_json)
        .into_diagnostic()?;
    let encoded_solution = {
        let builder = TychoRouterEncoderBuilder::new()
            .chain(chain)
            .swap_encoder_registry(swap_encoder_registry);

        builder
            .build()
            .into_diagnostic()
            .wrap_err("Failed to build encoder")?
            .encode_solutions(vec![solution.clone()])
            .into_diagnostic()
            .wrap_err("Failed to encode router calldata")?
            .into_iter()
            .next()
            .ok_or_else(|| miette!("Missing solution"))?
    };
    let transaction =
        encoded_transaction(encoded_solution.clone(), &solution, chain.native_token().address)?;
    Ok((solution, transaction))
}

pub fn create_solution(
    component: ProtocolComponent,
    state: Option<Arc<dyn ProtocolSim>>,
    sell_token: Token,
    buy_token: Token,
    amount_in: BigUint,
    gas_usage: BigUint,
    expected_amount_out: BigUint,
) -> miette::Result<Solution> {
    let user_address = Bytes::from_str(USER_ADDR).into_diagnostic()?;

    // Prepare data to encode. First we need to create a swap object
    let simple_swap = {
        let mut swap = Swap::new(component, sell_token.clone(), buy_token.clone(), gas_usage)
            .with_estimated_amount_in(amount_in.clone());

        if let Some(state) = state {
            swap = swap.with_protocol_state(state);
        }
        swap
    };

    // The widest tolerance the router accepts (20% below the quote) — these tests care about
    // whether the swap executes, not about a tight slippage bound.
    let min_amount_out = &expected_amount_out * BigUint::from(8000u64) / BigUint::from(10_000u64);

    Ok(Solution::new(
        user_address.clone(),
        user_address,
        sell_token.address,
        buy_token.address,
        amount_in,
        expected_amount_out,
        min_amount_out,
        vec![simple_swap],
    ))
}

fn encoded_transaction(
    encoded_solution: EncodedSolution,
    solution: &Solution,
    native_address: Bytes,
) -> miette::Result<Transaction> {
    let amount_in = biguint_to_u256(solution.amount_in());
    let amount_out = biguint_to_u256(solution.expected_amount_out());
    let min_amount_out = biguint_to_u256(solution.min_amount_out());
    let router_eth = Address::from_slice(ROUTER_ETH_ADDRESS.as_ref());
    let to_router_address = |raw: Address| {
        if raw.as_slice() == native_address.as_ref() {
            router_eth
        } else {
            raw
        }
    };

    let token_in = to_router_address(Address::from_slice(solution.token_in()));
    let token_out = to_router_address(Address::from_slice(solution.token_out()));
    let receiver = Address::from_slice(solution.receiver());
    let client_fee_params = ClientFeeParams::default().into_abi_params();

    let method_calldata = (
        amount_in,
        token_in,
        token_out,
        amount_out,
        min_amount_out,
        receiver,
        client_fee_params,
        encoded_solution.swaps(),
    )
        .abi_encode();

    let contract_interaction = encode_input(encoded_solution.function_signature(), method_calldata);
    let value = if *solution.token_in() == native_address {
        solution.amount_in().clone()
    } else {
        BigUint::ZERO
    };
    Ok(Transaction::new(
        encoded_solution
            .interacting_with()
            .clone(),
        value,
        contract_interaction,
        encoded_solution.estimated_gas().clone(),
    ))
}

/// Encodes the input data for a function call to the given function selector.
fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(selector.as_bytes());
    let selector_bytes = &hasher.finalize()[..4];
    let mut call_data = selector_bytes.to_vec();
    // Remove extra prefix if present (32 bytes for dynamic data)
    // Alloy encoding is including a prefix for dynamic data indicating the offset or length
    // but at this point we don't want that
    if encoded_args.len() > 32 &&
        encoded_args[..32] ==
            [0u8; 31]
                .into_iter()
                .chain([32].to_vec())
                .collect::<Vec<u8>>()
    {
        encoded_args = encoded_args[32..].to_vec();
    }
    call_data.extend(encoded_args);
    call_data
}

/// Detects balance and allowance storage slots for all given tokens in a single batch operation.
///
/// Returns a mapping from token address to their detected storage slots.
/// This function should be called once per block with all tokens of interest to optimize RPC calls.
/// Tokens that fail slot detection are silently skipped and not included in the result.
pub(crate) async fn detect_token_slots(
    rpc_tools: &RPCTools,
    token_addresses: &[Bytes],
    to_address: &Bytes,
) -> HashMap<Bytes, TokenSlots> {
    let user_address = match Address::from_str(USER_ADDR).into_diagnostic() {
        Ok(addr) => addr,
        Err(_) => return HashMap::new(),
    };

    let mut token_slots = HashMap::new();
    // Add one entry for the native token (represented as zero address)
    token_slots.insert(Bytes::zero(20), TokenSlots::default());

    // Filter out the native token (zero address) as it doesn't need slot detection
    let erc20_tokens: Vec<Bytes> = token_addresses
        .iter()
        .filter(|&addr| addr != &Bytes::zero(20))
        .cloned()
        .collect();

    if erc20_tokens.is_empty() {
        return token_slots;
    }

    let balance_results = rpc_tools
        .evm_balance_slot_detector
        .detect_balance_slots(&erc20_tokens, (**user_address).into())
        .await;

    let allowance_results = rpc_tools
        .evm_allowance_slot_detector
        .detect_allowance_slots(&erc20_tokens, (**user_address).into(), to_address.clone())
        .await;

    for token_address in &erc20_tokens {
        let balance_slot_data = match balance_results.get(token_address) {
            Some(Ok((storage_addr, slot))) => (storage_addr.clone(), slot.clone()),
            Some(Err(e)) => {
                tracing::warn!(token=%token_address, error=?e, "Balance slot detection failed");
                continue;
            }
            None => {
                tracing::warn!(token=%token_address, "Balance slot detection returned no result");
                continue;
            }
        };

        let allowance_slot_data = match allowance_results.get(token_address) {
            Some(Ok((storage_addr, slot))) => (storage_addr.clone(), slot.clone()),
            Some(Err(e)) => {
                tracing::warn!(token=%token_address, error=?e, "Allowance slot detection failed");
                continue;
            }
            None => {
                tracing::warn!(token=%token_address, "Allowance slot detection returned no result");
                continue;
            }
        };

        token_slots.insert(
            token_address.clone(),
            TokenSlots {
                balance_storage_addr: balance_slot_data.0.to_vec(),
                balance_slot: balance_slot_data.1.to_vec(),
                allowance_storage_addr: allowance_slot_data.0.to_vec(),
                allowance_slot: allowance_slot_data.1.to_vec(),
            },
        );
    }

    token_slots
}

/// Set up all state overrides needed for simulation using pre-computed token slots.
///
/// This includes balance overrides and allowance overrides of the sell token for the sender.
/// Returns both the overwrites and metadata for human-readable logging.
pub(crate) fn setup_user_overwrites(
    to_address: &Bytes,
    token_address: &Bytes,
    amount: &BigUint,
    token_slots: &TokenSlots,
) -> (AddressHashMap<AccountOverride>, OverwriteMetadata) {
    let mut overwrites = AddressHashMap::default();
    let mut metadata = OverwriteMetadata::new();
    let user_address = Address::from_str(USER_ADDR).expect("Valid user address");
    let spender_address = Address::from_slice(&to_address[..20]);

    // Native token (zero address)
    if token_address == &Bytes::zero(20) {
        // amount is sent as tx value, so the balance must cover both the swap value and gas
        let native_balance = biguint_to_u256(amount) + GAS_RESERVE;
        overwrites.insert(user_address, AccountOverride::default().with_balance(native_balance));
    } else {
        let token_balance = biguint_to_u256(amount);
        let token_allowance = biguint_to_u256(amount);

        let balance_storage_address = Address::from_slice(&token_slots.balance_storage_addr[..20]);
        let allowance_storage_address =
            Address::from_slice(&token_slots.allowance_storage_addr[..20]);

        let balance_slot_b256 = alloy::primitives::B256::from_slice(&token_slots.balance_slot);
        let allowance_slot_b256 = alloy::primitives::B256::from_slice(&token_slots.allowance_slot);

        debug!(
            "Setting token override for {token_address}: balance={}, allowance={}, balance_storage={}, allowance_storage={}",
            token_balance, token_allowance, balance_storage_address, allowance_storage_address
        );

        // Add metadata for human-readable logging
        metadata.add_balance(balance_storage_address, user_address, balance_slot_b256);
        metadata.add_allowance(
            allowance_storage_address,
            user_address,
            spender_address,
            allowance_slot_b256,
        );

        // Apply balance and allowance overrides
        // If both storage addresses are the same, combine them into one override
        if balance_storage_address == allowance_storage_address {
            overwrites.insert(
                balance_storage_address,
                AccountOverride::default().with_state_diff(vec![
                    (
                        balance_slot_b256,
                        alloy::primitives::B256::from_slice(&token_balance.to_be_bytes::<32>()),
                    ),
                    (
                        allowance_slot_b256,
                        alloy::primitives::B256::from_slice(&token_allowance.to_be_bytes::<32>()),
                    ),
                ]),
            );
        } else {
            // Different storage addresses, apply separately
            overwrites.insert(
                balance_storage_address,
                AccountOverride::default().with_state_diff(vec![(
                    balance_slot_b256,
                    alloy::primitives::B256::from_slice(&token_balance.to_be_bytes::<32>()),
                )]),
            );
            overwrites.insert(
                allowance_storage_address,
                AccountOverride::default().with_state_diff(vec![(
                    allowance_slot_b256,
                    alloy::primitives::B256::from_slice(&token_allowance.to_be_bytes::<32>()),
                )]),
            );
        }
        overwrites.insert(user_address, AccountOverride::default().with_balance(GAS_RESERVE));
    }

    (overwrites, metadata)
}

pub(crate) fn swap_request(
    transaction: &Transaction,
    block: &Block,
) -> miette::Result<TransactionRequest> {
    let (max_fee_per_gas, max_priority_fee_per_gas) = calculate_gas_fees(block)?;
    let user_address = Address::from_str(USER_ADDR).expect("Valid user address");
    Ok(TransactionRequest::default()
        .to(Address::from_slice(&transaction.to()[..20]))
        .input(transaction.data().clone().into())
        .value(U256::from_str(&transaction.value().to_string()).unwrap_or_default())
        .from(user_address)
        .gas_limit(GAS_LIMIT)
        .max_fee_per_gas(
            max_fee_per_gas
                .try_into()
                .unwrap_or(u128::MAX),
        )
        .max_priority_fee_per_gas(
            max_priority_fee_per_gas
                .try_into()
                .unwrap_or(u128::MAX),
        ))
}

/// Calculate gas fees based on block base fee
fn calculate_gas_fees(block: &Block) -> miette::Result<(U256, U256)> {
    let base_fee = block
        .header
        .base_fee_per_gas
        .ok_or_else(|| miette::miette!("Block does not have base fee (pre-EIP-1559)"))?;
    // Set max_priority_fee_per_gas to a reasonable value (2 Gwei)
    let max_priority_fee_per_gas = U256::from(2_000_000_000u64);
    // Set max_fee_per_gas to base_fee * 2 + max_priority_fee_per_gas to handle fee fluctuations
    let max_fee_per_gas = U256::from(base_fee) * U256::from(2u64) + max_priority_fee_per_gas;
    debug!(
        "Gas pricing: base_fee={}, max_priority_fee_per_gas={}, max_fee_per_gas={}",
        base_fee, max_priority_fee_per_gas, max_fee_per_gas
    );
    Ok((max_fee_per_gas, max_priority_fee_per_gas))
}

/// Calculate storage slot for Solidity mapping.
///
/// The solidity code:
/// keccak256(abi.encodePacked(bytes32(key), bytes32(slot)))
pub fn calculate_executor_storage_slot(key: Address) -> FixedBytes<32> {
    // Convert key (20 bytes) to 32-byte left-padded array (uint256)
    let mut key_bytes = [0u8; 32];
    key_bytes[12..].copy_from_slice(key.as_slice());

    // Storage layout (from `forge inspect TychoRouterV3 storageLayout`):
    //   slot 0: _roles             (AccessControl)
    //   slot 1: _balances          (ERC6909)
    //   slot 2: _operatorApprovals (ERC6909)
    //   slot 3: _allowances        (ERC6909)
    //   slot 4: _paused            (Pausable)
    //   slot 5: _vaultBalances     (Vault)
    //   slot 6: executorsActivationTimestamp (Dispatcher)
    let slot = U256::from(6);

    // Convert U256 slot to 32-byte big-endian array
    let slot_bytes = slot.to_be_bytes::<32>();

    // Concatenate key_bytes + slot_bytes, then keccak hash
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&key_bytes);
    buf[32..].copy_from_slice(&slot_bytes);
    keccak256(buf)
}

/// Sets up Angstrom-specific storage overwrites for simulation.
///
/// This function creates storage overwrites specifically for Angstrom hooks to ensure
/// proper simulation behavior. It sets the _lastBlockUpdated storage parameter to
/// unlock the pool in the simulator.
///
/// # Arguments
/// * `angstrom_address` - The address of the Angstrom hook contract
///   (0x0000000AA8c2Fb9b232F78D2B286dC2aE53BfAD4)
/// * `current_block_number` - The current block number to set as _lastBlockUpdated
///
/// # Returns
/// A HashMap containing account overwrites for the Angstrom contract.
/// The override includes:
///   - Storage slot 3, offset 0, bytes 8: Sets _lastBlockUpdated to current block number
pub fn setup_angstrom_overwrites(
    angstrom_address: Address,
    current_block_number: u64,
) -> AddressHashMap<AccountOverride> {
    let mut overwrites = AddressHashMap::default();

    // Angstrom storage slot 3, offset 0, 8 bytes for _lastBlockUpdated
    let storage_slot = alloy::primitives::B256::from([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 3,
    ]);

    // Use the actual storage pattern and only update the block number at the end
    // Pattern: 0x00000000e40df67976149fe316ca8437300da6fec92629ea00000000016d620b
    let mut storage_value =
        hex::decode("00000000e40df67976149fe316ca8437300da6fec92629ea00000000016d620b").unwrap();
    storage_value[24..32].copy_from_slice(&current_block_number.to_be_bytes());

    let storage_value_b256 = alloy::primitives::B256::from_slice(&storage_value);
    overwrites.insert(
        angstrom_address,
        AccountOverride::default().with_state_diff(vec![(storage_slot, storage_value_b256)]),
    );

    overwrites
}

/// Sets up FermiSwap registry storage overwrites for simulation.
///
/// FermiSwap reads oracle state from PrioUpdateRegistry using a lane keyed by
/// `keccak256(abi.encode(target, laneIndex))`. The first 4 bytes of that lane's
/// slot store the update timestamp. To keep the lane payload intact, this reads
/// each current slot value at the simulation block and only replaces the
/// timestamp prefix with `block.timestamp`.
pub async fn setup_fermiswap_overwrites(
    rpc_tools: &RPCTools,
    block: &Block,
    pairs: &[(Address, Address)],
) -> miette::Result<AddressHashMap<AccountOverride>> {
    let registry_address = Address::from_str(FERMISWAP_REGISTRY_ADDRESS).into_diagnostic()?;
    let target_address = Address::from_str(FERMISWAP_TARGET_ADDRESS).into_diagnostic()?;
    let timestamp = u32::try_from(block.header.timestamp)
        .map_err(|_| miette!("Block timestamp {} exceeds uint32", block.header.timestamp))?;
    let block_id = if block.header.hash == B256::ZERO {
        BlockId::from(BlockNumberOrTag::Pending)
    } else {
        BlockId::from(block.number())
    };

    let mut state_diff = Vec::new();
    for &(base_asset, quote_asset) in pairs {
        let lane_index = calculate_fermiswap_lane_index(base_asset, quote_asset);
        let storage_slot = calculate_fermiswap_registry_storage_slot(target_address, lane_index);
        let stored_value = rpc_tools
            .provider
            .get_storage_at(registry_address, U256::from_be_slice(storage_slot.as_slice()))
            .block_id(block_id)
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "Failed to fetch FermiSwap registry storage slot 0x{storage_slot:x} for pair {base_asset:?}/{quote_asset:?}"
                )
            })?;
        let storage_value = overwrite_fermiswap_lane_timestamp(
            B256::from_slice(&stored_value.to_be_bytes::<32>()),
            timestamp,
        );

        state_diff.push((storage_slot, storage_value));
    }

    let mut overwrites = AddressHashMap::default();
    if !state_diff.is_empty() {
        overwrites.insert(registry_address, AccountOverride::default().with_state_diff(state_diff));
    }
    Ok(overwrites)
}

fn calculate_fermiswap_lane_index(base_asset: Address, quote_asset: Address) -> B256 {
    // Mirrors `keccak256(abi.encode(baseAsset, quoteAsset))`.
    let mut encoded = [0u8; 64];
    encoded[12..32].copy_from_slice(base_asset.as_slice());
    encoded[44..64].copy_from_slice(quote_asset.as_slice());
    keccak256(encoded)
}

fn calculate_fermiswap_registry_storage_slot(target: Address, lane_index: B256) -> B256 {
    // Mirrors `keccak256(abi.encode(target, laneIndex))`.
    let mut encoded = [0u8; 64];
    encoded[12..32].copy_from_slice(target.as_slice());
    encoded[32..64].copy_from_slice(lane_index.as_slice());
    keccak256(encoded)
}

fn overwrite_fermiswap_lane_timestamp(stored_value: B256, timestamp: u32) -> B256 {
    let mut value = [0u8; 32];
    value.copy_from_slice(stored_value.as_slice());
    value[..4].copy_from_slice(&timestamp.to_be_bytes());
    B256::from(value)
}

/// Sets up BopAMM registry storage overwrites for simulation.
///
/// BopAMM prices each book from the same PrioUpdateRegistry as FermiSwap, but its lane is keyed
/// by `keccak256(abi.encode(module, assetId))` — the pricing module is the registry `target` and
/// the book's `assetId` is the lane index. The first 4 bytes of that lane's slot store the update
/// timestamp; the registry's exact-window `getState` gate reverts (`StaleUpdate`) unless
/// `block.timestamp` matches it. This reads each lane's current value at the simulation block and
/// only replaces the timestamp prefix with `block.timestamp`, leaving the quote payload intact.
pub async fn setup_bopamm_overwrites(
    rpc_tools: &RPCTools,
    block: &Block,
    asset_ids: &[U256],
) -> miette::Result<AddressHashMap<AccountOverride>> {
    let registry_address = Address::from_str(BOPAMM_REGISTRY_ADDRESS).into_diagnostic()?;
    let module_address = Address::from_str(BOPAMM_MODULE_ADDRESS).into_diagnostic()?;
    let timestamp = u32::try_from(block.header.timestamp)
        .map_err(|_| miette!("Block timestamp {} exceeds uint32", block.header.timestamp))?;
    let block_id = if block.header.hash == B256::ZERO {
        BlockId::from(BlockNumberOrTag::Pending)
    } else {
        BlockId::from(block.number())
    };

    let mut state_diff = Vec::new();
    for asset_id in asset_ids {
        let storage_slot = calculate_bopamm_registry_storage_slot(module_address, *asset_id);
        let stored_value = rpc_tools
            .provider
            .get_storage_at(registry_address, U256::from_be_slice(storage_slot.as_slice()))
            .block_id(block_id)
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "Failed to fetch BopAMM registry storage slot 0x{storage_slot:x} for asset id {asset_id}"
                )
            })?;
        let storage_value = overwrite_bopamm_lane_timestamp(
            B256::from_slice(&stored_value.to_be_bytes::<32>()),
            timestamp,
        );

        state_diff.push((storage_slot, storage_value));
    }

    let mut overwrites = AddressHashMap::default();
    if !state_diff.is_empty() {
        overwrites.insert(registry_address, AccountOverride::default().with_state_diff(state_diff));
    }
    Ok(overwrites)
}

fn calculate_bopamm_registry_storage_slot(target: Address, asset_id: U256) -> B256 {
    // Mirrors `keccak256(abi.encode(target, laneIndex))` where `laneIndex == assetId`.
    let mut encoded = [0u8; 64];
    encoded[12..32].copy_from_slice(target.as_slice());
    encoded[32..64].copy_from_slice(&asset_id.to_be_bytes::<32>());
    keccak256(encoded)
}

fn overwrite_bopamm_lane_timestamp(stored_value: B256, timestamp: u32) -> B256 {
    let mut value = [0u8; 32];
    value.copy_from_slice(stored_value.as_slice());
    value[..4].copy_from_slice(&timestamp.to_be_bytes());
    B256::from(value)
}

/// Builds the state overwrites described by `data` for the router at `router_address`.
///
/// Every executor in `data.executors` gets its `executorsActivationTimestamp` slot set to `1` on
/// the router, so `Dispatcher._validateExecutor` accepts it even when it is unapproved or still
/// inside its 3-day activation timelock. Executors mapped to `Some(bytecode)` additionally get
/// that bytecode planted at their address; the others keep their deployed code. Router and fee
/// calculator bytecode are planted only when present, and the fee calculator additionally gets the
/// router's `_feeCalculator` slot pointed at it.
///
/// Returns an empty map for `RouterOverwritesData::default()`, i.e. the deployed contracts are
/// simulated unchanged.
///
/// Intended for read-only simulation (`eth_call`/`debug_traceCall`) — a real transaction cannot
/// carry state overwrites.
pub fn setup_router_overwrites(
    router_address: Address,
    data: RouterOverwritesData,
) -> miette::Result<AddressHashMap<AccountOverride>> {
    let mut state_overwrites = AddressHashMap::default();
    if data.router_bytecode.is_none() &&
        data.executors.is_empty() &&
        data.fee_calculator_bytecode.is_none()
    {
        return Ok(state_overwrites);
    }

    let mut router_override = AccountOverride::default();
    let mut router_state_diff: Vec<(FixedBytes<32>, FixedBytes<32>)> = Vec::new();

    if let Some(router_bytecode) = data.router_bytecode {
        router_override = router_override.with_code(router_bytecode);
    }

    for (executor_address, executor_bytecode) in data.executors {
        router_state_diff.push((
            calculate_executor_storage_slot(executor_address),
            FixedBytes::<32>::from(U256::ONE),
        ));
        if let Some(executor_bytecode) = executor_bytecode {
            state_overwrites
                .insert(executor_address, AccountOverride::default().with_code(executor_bytecode));
        }
    }

    // The FeeCalculator override returns zero fees for all clients, so it acts as a no-op.
    if let Some(fee_calculator_bytecode) = data.fee_calculator_bytecode {
        let fee_calculator_address = Address::from_str(FEE_CALCULATOR_ADDRESS).into_diagnostic()?;
        // Storage layout slot 9 = _feeCalculator (see `forge inspect TychoRouterV3 storageLayout`)
        let fee_calculator_slot = FixedBytes::<32>::from(U256::from(9));
        let mut fee_calculator_slot_value = [0u8; 32];
        fee_calculator_slot_value[12..].copy_from_slice(fee_calculator_address.as_slice());
        router_state_diff
            .push((fee_calculator_slot, FixedBytes::<32>::from(fee_calculator_slot_value)));
        state_overwrites.insert(
            fee_calculator_address,
            AccountOverride::default().with_code(fee_calculator_bytecode),
        );
    }

    state_overwrites.insert(router_address, router_override.with_state_diff(router_state_diff));
    Ok(state_overwrites)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fermiswap_lane_timestamp_preserves_payload() {
        let stored_value = B256::from_slice(
            &hex::decode("1111111122222222333333334444444455555555666666667777777788888888")
                .unwrap(),
        );
        let updated = overwrite_fermiswap_lane_timestamp(stored_value, 0x01020304);

        let updated_bytes = updated.as_slice();
        assert_eq!(&updated_bytes[..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&updated_bytes[4..], &stored_value.as_slice()[4..]);
    }

    #[test]
    fn test_fermiswap_storage_slot_matches_solidity_layout() {
        let target = Address::from_str(FERMISWAP_TARGET_ADDRESS).unwrap();
        let weth = Address::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();

        let lane_index = calculate_fermiswap_lane_index(weth, usdc);
        let storage_slot = calculate_fermiswap_registry_storage_slot(target, lane_index);

        let expected_lane_index = keccak256((weth, usdc).abi_encode());
        let expected_storage_slot =
            keccak256((target, U256::from_be_slice(lane_index.as_slice())).abi_encode());

        assert_eq!(lane_index, expected_lane_index);
        assert_eq!(storage_slot, expected_storage_slot);
    }

    #[test]
    fn test_bopamm_lane_timestamp_preserves_payload() {
        let stored_value = B256::from_slice(
            &hex::decode("1111111122222222333333334444444455555555666666667777777788888888")
                .unwrap(),
        );
        let updated = overwrite_bopamm_lane_timestamp(stored_value, 0x01020304);

        let updated_bytes = updated.as_slice();
        assert_eq!(&updated_bytes[..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&updated_bytes[4..], &stored_value.as_slice()[4..]);
    }

    #[test]
    fn test_bopamm_storage_slot_matches_solidity_layout() {
        let module = Address::from_str(BOPAMM_MODULE_ADDRESS).unwrap();
        let asset_id = U256::from(2);

        let storage_slot = calculate_bopamm_registry_storage_slot(module, asset_id);
        let expected_storage_slot = keccak256((module, asset_id).abi_encode());

        assert_eq!(storage_slot, expected_storage_slot);
    }

    #[test]
    fn test_router_overwrites_activate_executors_without_touching_bytecode() {
        let router = Address::from_str("0x1234567890123456789012345678901234567890").unwrap();
        let executors = [
            Address::from_str("0x0017c8351b0e79ae4c0eeaa1c14dcdbc2b6f7e91").unwrap(),
            Address::from_str("0x667cb0d1e2c48d4b1d2a1f6e9c8f0a3b5d7e9f11").unwrap(),
        ];
        let data = RouterOverwritesData {
            executors: executors
                .iter()
                .map(|executor| (*executor, None))
                .collect(),
            ..Default::default()
        };

        let overwrites = setup_router_overwrites(router, data).unwrap();

        assert_eq!(overwrites.len(), 1, "only the router account must be overwritten");
        let router_override = overwrites
            .get(&router)
            .expect("router override");
        assert!(router_override.code.is_none(), "the router must keep its deployed bytecode");
        let state_diff = router_override
            .state_diff
            .as_ref()
            .expect("activation slots");
        assert_eq!(state_diff.len(), executors.len(), "one slot per executor");
        for executor in executors {
            assert_eq!(
                state_diff.get(&calculate_executor_storage_slot(executor)),
                Some(&FixedBytes::<32>::from(U256::ONE)),
                "executor {executor} is not activated"
            );
        }
    }

    #[test]
    fn test_router_overwrites_plant_bytecode_and_activate_the_executor() {
        let router = Address::from_str("0x1234567890123456789012345678901234567890").unwrap();
        let executor = Address::from_str(EXECUTOR_ADDRESS).unwrap();
        let fee_calculator = Address::from_str(FEE_CALCULATOR_ADDRESS).unwrap();
        let data = RouterOverwritesData {
            router_bytecode: Some(vec![0x60, 0x01]),
            executors: HashMap::from([(executor, Some(vec![0x60, 0x02]))]),
            fee_calculator_bytecode: Some(vec![0x60, 0x03]),
        };

        let overwrites = setup_router_overwrites(router, data).unwrap();

        assert_eq!(overwrites.len(), 3);
        assert_eq!(
            overwrites
                .get(&executor)
                .and_then(|o| o.code.as_ref()),
            Some(&alloy::primitives::Bytes::from(vec![0x60, 0x02]))
        );
        assert_eq!(
            overwrites
                .get(&fee_calculator)
                .and_then(|o| o.code.as_ref()),
            Some(&alloy::primitives::Bytes::from(vec![0x60, 0x03]))
        );

        let router_override = overwrites
            .get(&router)
            .expect("router override");
        assert_eq!(router_override.code, Some(alloy::primitives::Bytes::from(vec![0x60, 0x01])));
        let state_diff = router_override
            .state_diff
            .as_ref()
            .expect("state diff");
        assert_eq!(
            state_diff.get(&calculate_executor_storage_slot(executor)),
            Some(&FixedBytes::<32>::from(U256::ONE)),
            "the executor is not activated"
        );
        let mut fee_calculator_slot_value = [0u8; 32];
        fee_calculator_slot_value[12..].copy_from_slice(fee_calculator.as_slice());
        assert_eq!(
            state_diff.get(&FixedBytes::<32>::from(U256::from(9))),
            Some(&FixedBytes::<32>::from(fee_calculator_slot_value)),
            "the router does not point at the overwritten fee calculator"
        );
    }

    #[test]
    fn test_default_router_overwrites_leave_every_contract_deployed() {
        let router = Address::from_str("0x1234567890123456789012345678901234567890").unwrap();

        let overwrites = setup_router_overwrites(router, RouterOverwritesData::default()).unwrap();

        assert!(overwrites.is_empty());
    }
}
