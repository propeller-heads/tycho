#![allow(dead_code)] // TODO: Remove this

// This module provides hook entrypoint generation for Uniswap V4 hooks.

use std::collections::{BTreeMap, HashMap};

use alloy::{
    core::sol,
    primitives::{hex, U256},
    sol_types::{
        private::{
            primitives::aliases::{I24, U24},
            Address as SolAddress, Bytes as SolBytes,
        },
        SolValue,
    },
};
use async_trait::async_trait;
use num_bigint::BigInt;
use num_traits::Zero;
use thiserror::Error;
use tracing::{debug, error, info, instrument, warn};
use tycho_common::{
    keccak256,
    models::{
        blockchain::{
            AccountOverrides, Block, EntryPoint, EntryPointWithTracingParams, RPCTracerParams,
            StorageOverride, TracingParams,
        },
        protocol::ProtocolComponent,
        Address,
    },
    traits::BalanceSlotDetector,
    Bytes,
};

use super::component_metadata::ComponentTracingMetadata;
type SlotId = U256;

// V4MiniRouter compiled bytecode
// The V4MiniRouter is a simple wrapper around V4Router that:
// - Overrides msgSender() to return address(this)
// - Implements _pay() to burn tokens from the payer
// - Exposes execute(bytes calldata params) to call _executeActions
pub(super) const V4_MINI_ROUTER_BYTECODE: &[u8] = include_bytes!("assets/V4MiniRouter.evm.runtime");
pub(super) const UNICHAIN_V4_MINI_ROUTER_BYTECODE: &[u8] =
    include_bytes!("assets/UnichainV4MiniRouter.evm.runtime");

// V4Router action constants
// These correspond to the actions enum in V4Router
const ACTION_SWAP_EXACT_IN_SINGLE: u8 = 6; // SWAP_EXACT_IN_SINGLE = 0x06
const ACTION_SETTLE_ALL: u8 = 12; // SETTLE_ALL = 0x0c
const ACTION_TAKE_ALL: u8 = 15; // TAKE_ALL = 0x0f

// V4Router function selector for execute(bytes calldata params)
// This is the keccak256 hash of "execute(bytes)" truncated to 4 bytes: 0x09c5eabe
const EXECUTE_FUNCTION_SELECTOR: [u8; 4] = [0x09, 0xc5, 0xea, 0xbe];

sol! {
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct ExactInputSingleParams {
        PoolKey poolKey;
        bool zeroForOne;
        uint128 amountIn;
        uint128 amountOutMinimum;
        bytes hookData;
    }

    struct Plan {
        bytes actions;
        bytes[] params;
    }

    function execute(bytes calldata params) public;
}

/// Configuration for hook entrypoint generation
pub(super) struct HookEntrypointConfig {
    /// Ideal number of entrypoints to generate for each component.
    max_sample_size: Option<usize>,
    /// Minimum number of entrypoints to generate for each component.
    min_samples: usize,
    /// Router address to use for the entrypoints. If not provided, uses a random address.
    router_address: Option<Address>,
    /// Sender address for transactions. If not provided, uses a default address.
    /// Could be defined by a custom Hook Orchestrator.
    sender: Option<Address>,
    /// Router bytecode to use for state overrides. If not provided, uses V4MiniRouter bytecode.
    router_code: Option<Bytes>,
    /// Pool manager address (required)
    pool_manager: Address,
}

impl HookEntrypointConfig {
    pub(super) fn new(
        max_sample_size: Option<usize>,
        min_samples: usize,
        router_address: Option<Address>,
        sender: Option<Address>,
        router_code: Option<Bytes>,
        pool_manager: Address,
    ) -> Self {
        Self { max_sample_size, min_samples, router_address, sender, router_code, pool_manager }
    }
}

/// Data required for generating hook entrypoints
pub(super) struct HookEntrypointData {
    /// The address of the hook contract
    hook_address: Address,
    /// Component should provide, via static attributes - all the information required for PoolKey.
    /// PoolKey is generated from tokens, LPfee, tickSpacing and hooks address.
    /// https://github.com/Uniswap/v4-core/blob/main/src/types/PoolKey.sol
    component: ProtocolComponent,
    /// Metadata for component tracing (balances, limits, etc.)
    component_metadata: ComponentTracingMetadata,
    /// Whether to use balance slot overwrites for ERC20 tokens
    use_balance_overwrites: bool,
}

impl HookEntrypointData {
    pub(super) fn new(
        component: ProtocolComponent,
        component_metadata: ComponentTracingMetadata,
        hook_address: Address,
        use_balance_overwrites: bool,
    ) -> Self {
        Self { component, component_metadata, hook_address, use_balance_overwrites }
    }
}

/// Context for hook tracing
pub(super) struct HookTracerContext {
    /// The block at which to trace
    block: Block,
}

impl HookTracerContext {
    pub(super) fn new(block: Block) -> Self {
        Self { block }
    }
}

#[derive(Debug, Clone, Error)]
pub(super) enum EntrypointGenerationError {
    /// Failed to estimate swap amounts
    #[error("Failed to estimate swap amounts: {0}")]
    AmountsEstimationFailed(String),
    /// Failed to generate entrypoints
    #[error("Failed to generate entrypoints: {0}")]
    EntrypointGenerationFailed(String),
    /// No data available for generation
    #[error("No data available: {0}")]
    NoDataAvailable(String),
}

/// Trait for generating hook entrypoints
#[allow(dead_code)]
#[async_trait]
pub(super) trait HookEntrypointGenerator<B>
where
    B: BalanceSlotDetector,
{
    /// Set the configuration for the generator
    fn set_config(&mut self, config: HookEntrypointConfig);

    /// Generate entrypoints for a given component.
    /// First, it should call the SwapAmountEstimator to estimate the swap amounts.
    /// Then, using the ERC6909Overwrites and the custom Router code, we should generate
    /// the Entrypoints for tracing the router's 0x09c5eabe function.
    async fn generate_entrypoints(
        &self,
        data: &HookEntrypointData,
        context: &HookTracerContext,
    ) -> Result<Vec<EntryPointWithTracingParams>, EntrypointGenerationError>;
}

/// Trait for estimating swap amounts for entrypoint generation
pub(super) trait SwapAmountEstimator {
    /// Estimate the swap amounts for a given component.
    /// If limits are available, use different fractions of it; use 1, 10, 50 and 95% of the limits,
    /// to cover different bands. Else, use fractions of balances.
    /// Else, raise a custom Error.
    ///
    /// Returns a combination of TokenIn and Token Out and an array of swap amounts, encoded in
    /// bytes
    fn estimate_swap_amounts(
        &self,
        metadata: &ComponentTracingMetadata,
        tokens: &[Address],
    ) -> Result<HashMap<(Address, Address), Vec<Bytes>>, EntrypointGenerationError>;
}

// Auxiliary code
fn get_storage_slot_index_at_key(
    key: U256,
    mapping_slot: SlotId,
    compiler: ContractCompiler,
) -> SlotId {
    let mut key_bytes = key.to_be_bytes::<32>().to_vec();
    if key_bytes.len() < 32 {
        let padding = vec![0u8; 32 - key_bytes.len()];
        key_bytes.splice(0..0, padding); // Prepend zeros to the start
    }

    let mapping_slot_bytes = mapping_slot.to_be_bytes::<32>();
    compiler.compute_map_slot(&mapping_slot_bytes, &key_bytes)
}

/// Enum representing the type of contract compiler.
#[derive(Debug, PartialEq, Copy, Clone)]
enum ContractCompiler {
    Solidity,
    Vyper,
}

impl ContractCompiler {
    /// Computes the storage slot for a given mapping based on the base storage slot of the map and
    /// the key.
    ///
    /// # Arguments
    ///
    /// * `map_base_slot` - A byte slice representing the base storage slot of the mapping.
    /// * `key` - A byte slice representing the key for which the storage slot is being computed.
    ///
    /// # Returns
    ///
    /// A `SlotId` representing the computed storage slot.
    ///
    /// # Notes
    ///
    /// - For `Solidity`, the slot is computed as `keccak256(key + map_base_slot)`.
    /// - For `Vyper`, the slot is computed as `keccak256(map_base_slot + key)`.
    pub(super) fn compute_map_slot(&self, map_base_slot: &[u8], key: &[u8]) -> SlotId {
        let concatenated = match &self {
            ContractCompiler::Solidity => [key, map_base_slot].concat(),
            ContractCompiler::Vyper => [map_base_slot, key].concat(),
        };

        let slot_bytes = keccak256(&concatenated);

        SlotId::from_be_slice(&slot_bytes)
    }
}
struct ERC6909Overwrites {
    balance_of_slot: SlotId,
    operator_slot: SlotId,
    allowance_slot: SlotId,
}

impl Default for ERC6909Overwrites {
    fn default() -> Self {
        ERC6909Overwrites {
            balance_of_slot: U256::try_from(4).unwrap(),
            operator_slot: U256::try_from(3).unwrap(),
            allowance_slot: U256::try_from(5).unwrap(),
        }
    }
}

impl ERC6909Overwrites {
    fn balance_slot(&self, owner: Address, token: Address) -> SlotId {
        let balance_mapping_slot = get_storage_slot_index_at_key(
            U256::from_be_slice(owner.as_ref()),
            self.balance_of_slot,
            ContractCompiler::Solidity,
        );
        get_storage_slot_index_at_key(
            U256::from_be_slice(token.as_ref()),
            balance_mapping_slot,
            ContractCompiler::Solidity,
        )
    }
}

/// Preferred method for estimating swap amounts
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum EstimationMethod {
    /// Prefer limits for estimation (if available)
    Limits,
    /// Prefer balances for estimation (if available)
    Balances,
}

/// Default implementation of SwapAmountEstimator
/// Can be configured to prefer either limits or balances
pub(super) struct DefaultSwapAmountEstimator {
    preferred_method: EstimationMethod,
}

impl DefaultSwapAmountEstimator {
    /// Create a new estimator with the specified preferred method
    pub(super) fn new(preferred_method: EstimationMethod) -> Self {
        Self { preferred_method }
    }

    /// Create an estimator that prefers limits
    pub(super) fn with_limits() -> Self {
        Self::new(EstimationMethod::Limits)
    }

    /// Create an estimator that prefers balances
    pub(super) fn with_balances() -> Self {
        Self::new(EstimationMethod::Balances)
    }
}

impl SwapAmountEstimator for DefaultSwapAmountEstimator {
    #[instrument(skip(self, metadata), fields(
        token_count = tokens.len(),
        has_limits = metadata.limits().is_some(),
        has_balances = metadata.balances().is_some(),
        preferred_method = ?self.preferred_method
    ))]
    fn estimate_swap_amounts(
        &self,
        metadata: &ComponentTracingMetadata,
        tokens: &[Address],
    ) -> Result<HashMap<(Address, Address), Vec<Bytes>>, EntrypointGenerationError> {
        debug!(
            "Starting swap amount estimation with preferred method: {:?}",
            self.preferred_method
        );

        // Check based on preferred method
        let result = match self.preferred_method {
            EstimationMethod::Balances => {
                // Try balances first
                let mut result = self.try_estimate_from_balances(metadata, tokens);
                if result.is_empty() {
                    // Fallback to limits
                    result = self.try_estimate_from_limits(metadata);
                }
                result
            }
            EstimationMethod::Limits => {
                // Try limits first
                let mut result = self.try_estimate_from_limits(metadata);
                if result.is_empty() {
                    // Fallback to balances
                    result = self.try_estimate_from_balances(metadata, tokens);
                }
                result
            }
        };

        if !result.is_empty() {
            return Ok(result);
        }

        // If neither limits nor balances are available, return error
        warn!("No usable limits or balances found for swap amount estimation");
        Err(EntrypointGenerationError::NoDataAvailable(
            "No limits or balances available for swap amount estimation".to_string(),
        ))
    }
}

impl DefaultSwapAmountEstimator {
    fn try_estimate_from_limits(
        &self,
        metadata: &ComponentTracingMetadata,
    ) -> HashMap<(Address, Address), Vec<Bytes>> {
        let mut result = HashMap::new();

        if let Some(Ok(limits)) = metadata.limits() {
            debug!(limit_count = limits.len(), "Found limits data, trying estimation");
            if !limits.is_empty() {
                let mut valid_limits = 0;
                let mut zero_limits = 0;

                for ((token0, token1), (limit0, _limit1, _)) in limits {
                    let limit_amount =
                        BigInt::from_bytes_be(num_bigint::Sign::Plus, limit0.as_ref());

                    if !limit_amount.is_zero() {
                        valid_limits += 1;

                        debug!(
                            token0 = %token0,
                            token1 = %token1,
                            limit_amount_hex = %format!("{:#x}", &limit_amount),
                            "Processing limit for token pair"
                        );
                        let hundred = BigInt::from(100);
                        let amounts = vec![
                            Bytes::from(
                                (&limit_amount / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 1%
                            Bytes::from(
                                ((&limit_amount * BigInt::from(10)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 10%
                            Bytes::from(
                                ((&limit_amount * BigInt::from(50)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 50%
                            Bytes::from(
                                ((&limit_amount * BigInt::from(95)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 95%
                        ];

                        debug!(
                            token0 = %token0,
                            token1 = %token1,
                            amount_count = amounts.len(),
                            "Generated amounts from limits"
                        );

                        result.insert((token0.clone(), token1.clone()), amounts);
                    } else {
                        zero_limits += 1;
                        debug!(
                            token0 = %token0,
                            token1 = %token1,
                            "Skipping zero limit"
                        );
                    }
                }

                info!(
                    valid_limits,
                    zero_limits,
                    total_pairs_generated = result.len(),
                    "Processed all limits"
                );
            }
        }

        result
    }

    fn try_estimate_from_balances(
        &self,
        metadata: &ComponentTracingMetadata,
        tokens: &[Address],
    ) -> HashMap<(Address, Address), Vec<Bytes>> {
        let mut result = HashMap::new();

        if let Some(Ok(balances)) = metadata.balances() {
            debug!(balance_count = balances.len(), "Found balance data, using for estimation");

            let mut tokens_with_balance = 0;
            let mut tokens_without_balance = 0;
            for sell_token in tokens {
                if let Some(balance_bytes) = balances.get(sell_token) {
                    let balance =
                        BigInt::from_bytes_be(num_bigint::Sign::Plus, balance_bytes.as_ref());
                    if !balance.is_zero() {
                        tokens_with_balance += 1;

                        debug!(
                            sell_token = %sell_token,
                            balance_hex = %format!("{:#x}", &balance),
                            "Processing balance for token"
                        );
                        let hundred = BigInt::from(100);
                        // Currently, Euler hooks fail if we try to swap more than 12.5% of the
                        // reserves
                        let amounts = vec![
                            Bytes::from((&balance / &hundred).to_bytes_be().1), // 1%
                            Bytes::from(
                                ((&balance * BigInt::from(2)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 2%
                            Bytes::from(
                                ((&balance * BigInt::from(5)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 5%
                            Bytes::from(
                                ((&balance * BigInt::from(10)) / &hundred)
                                    .to_bytes_be()
                                    .1,
                            ), // 10%
                        ];
                        // Create entries for each possible buy token
                        let mut pairs_created = 0;
                        for buy_token in tokens {
                            if sell_token != buy_token {
                                pairs_created += 1;
                                result.insert(
                                    (sell_token.clone(), buy_token.clone()),
                                    amounts.clone(),
                                );
                            }
                        }

                        debug!(
                            sell_token = %sell_token,
                            pairs_created,
                            "Created swap pairs from balance"
                        );
                    } else {
                        tokens_without_balance += 1;
                        debug!(
                            sell_token = %sell_token,
                            "Token has zero balance, skipping"
                        );
                    }
                } else {
                    tokens_without_balance += 1;
                    debug!(
                        sell_token = %sell_token,
                        "Token not found in balances, skipping"
                    );
                }
            }

            info!(
                tokens_with_balance,
                tokens_without_balance,
                total_pairs_generated = result.len(),
                "Processed all token balances"
            );
        }

        result
    }
}

/// Default implementation of HookEntrypointGenerator for Uniswap V4 hooks
/// Generates entrypoints using V4MiniRouter for tracing hook interactions
pub(super) struct UniswapV4DefaultHookEntrypointGenerator<E, B>
where
    E: SwapAmountEstimator,
    B: BalanceSlotDetector,
{
    config: HookEntrypointConfig,
    estimator: E,
    balance_slot_detector: B,
}

impl<E, B> UniswapV4DefaultHookEntrypointGenerator<E, B>
where
    E: SwapAmountEstimator,
    B: BalanceSlotDetector,
{
    pub(super) fn new(estimator: E, pool_manager: Address, balance_slot_detector: B) -> Self {
        Self {
            config: HookEntrypointConfig {
                max_sample_size: Some(4),
                min_samples: 1,
                router_address: None,
                sender: None,
                router_code: None,
                pool_manager,
            },
            estimator,
            balance_slot_detector,
        }
    }
}

#[async_trait]
impl<E, B> HookEntrypointGenerator<B> for UniswapV4DefaultHookEntrypointGenerator<E, B>
where
    E: SwapAmountEstimator + Send + Sync,
    B: BalanceSlotDetector + Send + Sync,
{
    fn set_config(&mut self, config: HookEntrypointConfig) {
        debug!("Updating hook entrypoint generator configuration");
        self.config = config;
    }

    #[instrument(skip(self, data, context), fields(
        component_id = %data.component.id,
        hook_address = %data.hook_address,
        token_count = data.component.tokens.len()
    ))]
    async fn generate_entrypoints(
        &self,
        data: &HookEntrypointData,
        context: &HookTracerContext,
    ) -> Result<Vec<EntryPointWithTracingParams>, EntrypointGenerationError> {
        info!("Starting entrypoint generation");

        let hook_address = data.hook_address.clone();
        let tokens = data.component.tokens.clone();

        // Ensure tokens are properly ordered according to UniswapV4 logic
        let (token0, token1) = if tokens[0] < tokens[1] {
            (tokens[0].clone(), tokens[1].clone())
        } else {
            (tokens[1].clone(), tokens[0].clone())
        };

        // Defaults to random predefined address.
        let router_address = self
            .config
            .router_address
            .clone()
            .unwrap_or_else(|| Address::from(hex!("1f31095ECb8dD97f7133cC9a4dD208b8645c4E24")));

        let max_sample_size = self.config.max_sample_size.unwrap_or(4);
        let min_samples = self.config.min_samples;

        debug!(
            router_address = %router_address,
            max_sample_size,
            min_samples,
            "Configuration for entrypoint generation"
        );

        let mut entrypoints = Vec::new();

        let swap_amounts = self
            .estimator
            .estimate_swap_amounts(&data.component_metadata, &tokens)
            .map_err(|e| {
                error!(
                    component_id = %data.component.id,
                    error = %e,
                    "Failed to estimate swap amounts"
                );
                e
            })?;

        info!(swap_pair_count = swap_amounts.len(), "Estimated swap amounts for token pairs");

        // Detect balance slots for tokens if balance slot detector is available
        // perf: If we move the extraction to the Hooks DCI, we can extract all the token slots at
        // once. This way we can leverage the Slot Detector's parallelization and improve
        // speed. For now, this was included on the EntrypointGenerator for quick
        // integration and to reduce the number of interface changes.
        let detected_balance_slots = if data.use_balance_overwrites {
            info!(
                component_id = &data.component.id,
                token_count = tokens.len(),
                "Detecting balance slots for component tokens"
            );
            let pool_manager = self.config.pool_manager.clone();

            let detection_results = self
                .balance_slot_detector
                .detect_balance_slots(&tokens, pool_manager)
                .await;

            // Convert token-based results to the expected format
            let mut token_slots = HashMap::new();
            for (token, result) in detection_results {
                match result {
                    Ok((_storage_addr, slot)) => {
                        token_slots.insert(token, slot);
                    }
                    Err(e) => {
                        let error_msg =
                            format!("Balance slot detection failed for token {}: {:?}", token, e);
                        error!(
                            component_id = %data.component.id,
                            hook_address = %data.hook_address,
                            token = %token,
                            error = %error_msg,
                            "Balance slot detection failed for token"
                        );
                        // Continue processing other tokens instead of failing completely
                    }
                }
            }

            // If balance slot detector was provided but no slots were successfully detected, fail
            if token_slots.is_empty() && !tokens.is_empty() {
                return Err(EntrypointGenerationError::NoDataAvailable(
                    "Balance slot detection failed for all tokens".to_string(),
                ));
            }

            token_slots
        } else {
            info!(
                component_id = %data.component.id,
                hook_address = %data.hook_address,
                "Skipping balance overwrites for component"
            );
            HashMap::new()
        };

        for ((token_in, token_out), amounts) in swap_amounts.iter() {
            debug!(
                token_in = %token_in,
                token_out = %token_out,
                amount_count = amounts.len(),
                "Processing token pair"
            );
            let sell_token = SolAddress::from_slice(token_in.as_ref());
            let buy_token = SolAddress::from_slice(token_out.as_ref());

            let token_0 = SolAddress::from_slice(token0.as_ref());
            let token_1 = SolAddress::from_slice(token1.as_ref());

            let hooks = SolAddress::from_slice(data.hook_address.as_ref());

            let fee = u32::from(
                data.component
                    .static_attributes
                    .get("key_lp_fee")
                    .ok_or_else(|| {
                        error!(
                            component_id = %data.component.id,
                            "Fee attribute not found for component"
                        );
                        EntrypointGenerationError::EntrypointGenerationFailed(
                            "Fee attribute not found for component".to_string(),
                        )
                    })?
                    .clone(),
            );

            let tick_spacing = i32::from(
                data.component
                    .static_attributes
                    .get("tick_spacing")
                    .ok_or_else(|| {
                        error!(
                            component_id = %data.component.id,
                            "tick_spacing attribute not found for component"
                        );
                        EntrypointGenerationError::EntrypointGenerationFailed(
                            "tick_spacing attribute not found for component".to_string(),
                        )
                    })?
                    .clone(),
            );

            debug!(fee, tick_spacing, "Extracted pool parameters from component");

            let pool_key = PoolKey {
                currency0: token_0,
                currency1: token_1,
                fee: U24::try_from(fee).expect("Fee value too large for U24"),
                tickSpacing: I24::try_from(tick_spacing)
                    .expect("tick_spacing value out of range for I24"),
                hooks,
            };

            // Determine swap direction: true if swapping token0 for token1
            let is_zero_for_one = token_in == &token0;

            let amounts_to_use = if amounts.len() >= max_sample_size {
                debug!(
                    token_in = %token_in,
                    token_out = %token_out,
                    "Using max sample size, taking {} out of {} amounts",
                    max_sample_size,
                    amounts.len()
                );
                amounts
                    .iter()
                    .take(max_sample_size)
                    .collect::<Vec<_>>()
            } else if amounts.len() >= min_samples {
                debug!(
                    token_in = %token_in,
                    token_out = %token_out,
                    "Using all {} available amounts",
                    amounts.len()
                );
                amounts.iter().collect::<Vec<_>>()
            } else {
                error!(
                    token_in = %token_in,
                    token_out = %token_out,
                    available_amounts = amounts.len(),
                    min_samples,
                    "Insufficient swap amounts for token pair"
                );
                return Err(EntrypointGenerationError::AmountsEstimationFailed(
                    format!(
                        "Insufficient swap amounts for token pair {:?} -> {:?}: got {}, need at least {}",
                        token_in, token_out, amounts.len(), min_samples
                    ),
                ));
            };

            for (amount_idx, amount_bytes) in amounts_to_use.iter().enumerate() {
                let amount_u256 = U256::from_be_slice(amount_bytes.as_ref());
                let amount_in = u128::try_from(amount_u256).map_err(|_| {
                    error!(
                        token0 = %token_in,
                        token1 = %token_out,
                        amount_hex = %format!("{:#x}", amount_u256),
                        "Amount too large for u128"
                    );
                    EntrypointGenerationError::EntrypointGenerationFailed(
                        "Amount too large for u128".to_string(),
                    )
                })?;

                debug!(
                    token_in = %token_in,
                    token_out = %token_out,
                    amount_idx,
                    amount_in,
                    amount_hex = %format!("{:#x}", amount_in),
                    "Processing swap amount"
                );

                let swap_params = ExactInputSingleParams {
                    poolKey: pool_key.clone(),
                    zeroForOne: is_zero_for_one,
                    amountIn: amount_in,
                    amountOutMinimum: 0,
                    // We only support composable hooks, meaning hook data is not supported atm.
                    hookData: Default::default(),
                };

                let actions = SolBytes::from(vec![
                    ACTION_SWAP_EXACT_IN_SINGLE,
                    ACTION_SETTLE_ALL,
                    ACTION_TAKE_ALL,
                ]);
                let params = vec![
                    SolBytes::from(swap_params.abi_encode()),
                    SolBytes::from((sell_token, amount_in).abi_encode()),
                    SolBytes::from((buy_token, 0u128).abi_encode()),
                ];

                let mut calldata = hex::decode("09c5eabe").unwrap();
                let mut tmp = (actions, params).abi_encode();

                // Remove first 32 bytes like the reference implementation
                tmp.drain(0..32);

                calldata.extend(tmp.abi_encode());

                // Always use ERC6909 overwrites for the router
                let overwrites = ERC6909Overwrites::default();
                let erc6909_balance_slot =
                    overwrites.balance_slot(router_address.clone(), token_in.clone());

                debug!(
                    token_in = %token_in,
                    erc6909_slot = %hex::encode(erc6909_balance_slot.to_be_bytes::<32>()),
                    "Using ERC6909 balance slot"
                );

                let pool_manager = self.config.pool_manager.clone();

                let router_code = self
                    .config
                    .router_code
                    .clone()
                    .unwrap_or_else(|| Bytes::from(V4_MINI_ROUTER_BYTECODE));

                let mut state_overrides = BTreeMap::new();

                // Check if token_in is the native token for this chain and set native balance
                // accordingly
                let native_token_address = context
                    .block
                    .chain
                    .native_token()
                    .address;
                let native_balance = if *token_in == native_token_address {
                    // Set native balance to amount_in (32-byte big-endian)
                    Some(Bytes::from(
                        U256::from(amount_in)
                            .to_be_bytes::<32>()
                            .as_slice(),
                    ))
                } else {
                    None
                };

                state_overrides.insert(
                    router_address.clone(),
                    AccountOverrides {
                        slots: None,
                        native_balance,
                        code: Some(router_code.clone()),
                    },
                );

                let mut pool_manager_storage_diff = BTreeMap::new();

                // Always set ERC6909 balance slot with amount_in
                pool_manager_storage_diff.insert(
                    Bytes::from(
                        erc6909_balance_slot
                            .to_be_bytes::<32>()
                            .as_slice(),
                    ),
                    Bytes::from(
                        U256::from(amount_in)
                            .to_be_bytes::<32>()
                            .as_slice(),
                    ),
                );

                state_overrides.insert(
                    pool_manager.clone(),
                    AccountOverrides {
                        slots: Some(StorageOverride::Diff(pool_manager_storage_diff)),
                        native_balance: None,
                        code: None,
                    },
                );

                // Additionally, if we have detected balance slots, also set those with amount_in *
                // 2 Capped to U256::MAX;
                if let Some(detected_slot) = detected_balance_slots.get(token_in) {
                    debug!(
                        token_in = %token_in,
                        detected_slot = %hex::encode(detected_slot),
                        "Also setting detected balance slot"
                    );
                    let mut token_in_storage_diff = BTreeMap::new();

                    token_in_storage_diff.insert(
                        detected_slot.clone(),
                        Bytes::from(
                            U256::from(amount_in.saturating_mul(2_u128))
                                .to_be_bytes::<32>()
                                .as_slice(),
                        ),
                    );
                    state_overrides.insert(
                        token_in.clone(),
                        AccountOverrides {
                            slots: Some(StorageOverride::Diff(token_in_storage_diff)),
                            native_balance: None,
                            code: None,
                        },
                    );
                }

                // Add zero address balance override to ensure sufficient funds for gas
                // When caller is None, RPC defaults to zero address as sender
                let zero_address = Address::from([0u8; 20]);
                let sufficient_balance_wei = U256::from(10_000_000_000_000_000_000u128); // 10 ETH
                state_overrides.insert(
                    zero_address,
                    AccountOverrides {
                        slots: None,
                        native_balance: Some(Bytes::from(
                            sufficient_balance_wei
                                .to_be_bytes::<32>()
                                .as_slice(),
                        )),
                        code: None,
                    },
                );

                let entry_point_id = format!("{hook_address}:execute(bytes)");
                debug!(
                    entry_point_id = %entry_point_id,
                    hook_address = %hook_address,
                    router_address = %router_address,
                    "Creating EntryPoint with ID"
                );

                let entry_point = EntryPointWithTracingParams::new(
                    EntryPoint::new(
                        entry_point_id.clone(),
                        router_address.clone(),
                        "execute(bytes)".to_string(),
                    ),
                    TracingParams::RPCTracer(
                        RPCTracerParams::new(None, Bytes::from(calldata))
                            .with_state_overrides(state_overrides)
                            .with_prune_addresses(vec![router_address.clone()]),
                    ),
                );

                debug!(
                    token_in = %token_in,
                    token_out = %token_out,
                    amount_idx,
                    entry_point_id = %entry_point_id,
                    "Generated entrypoint for swap"
                );

                entrypoints.push(entry_point);
            }
        }

        if entrypoints.is_empty() {
            warn!(
                component_id = %data.component.id,
                "No entrypoints could be generated"
            );
            return Err(EntrypointGenerationError::NoDataAvailable(
                "No entrypoints could be generated".to_string(),
            ));
        }

        info!(
            component_id = %data.component.id,
            entrypoint_count = entrypoints.len(),
            "Successfully generated entrypoints"
        );

        Ok(entrypoints)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    // Use the auto-generated mock
    use tycho_common::traits::MockBalanceSlotDetector;
    use tycho_common::{
        models::{protocol::ProtocolComponent, TxHash},
        Bytes,
    };

    use super::*;

    fn create_test_tokens() -> Vec<Address> {
        vec![Address::from([1u8; 20]), Address::from([2u8; 20])]
    }

    fn create_metadata_with_limits(
        limits: Vec<((Address, Address), (Bytes, Bytes))>,
    ) -> ComponentTracingMetadata {
        ComponentTracingMetadata::new(TxHash::from([0u8; 32])).with_limits(Ok(limits
            .into_iter()
            .map(|(tokens, (l0, l1))| (tokens, (l0, l1, None)))
            .collect()))
    }

    fn create_metadata_with_balances(
        balances: HashMap<Address, Bytes>,
    ) -> ComponentTracingMetadata {
        ComponentTracingMetadata::new(TxHash::from([0u8; 32])).with_balances(Ok(balances))
    }

    #[tokio::test]
    async fn test_estimate_with_limits() {
        let estimator = DefaultSwapAmountEstimator::with_limits();
        let tokens = create_test_tokens();

        let limit_value = BigInt::from(10000u64);
        let limits = vec![(
            (tokens[0].clone(), tokens[1].clone()),
            (Bytes::from(limit_value.to_bytes_be().1), Bytes::from(limit_value.to_bytes_be().1)),
        )];
        let metadata = create_metadata_with_limits(limits);

        let result = estimator
            .estimate_swap_amounts(&metadata, &tokens)
            .unwrap();

        assert_eq!(result.len(), 1);

        let amounts = result
            .get(&(tokens[0].clone(), tokens[1].clone()))
            .unwrap();
        assert_eq!(amounts.len(), 4);
        assert_eq!(amounts[0], Bytes::from(BigInt::from(100u64).to_bytes_be().1)); // 1%
        assert_eq!(amounts[1], Bytes::from(BigInt::from(1000u64).to_bytes_be().1)); // 10%
        assert_eq!(amounts[2], Bytes::from(BigInt::from(5000u64).to_bytes_be().1)); // 50%
        assert_eq!(amounts[3], Bytes::from(BigInt::from(9500u64).to_bytes_be().1));
        // 95%
    }

    #[tokio::test]
    async fn test_estimate_with_balances_fallback() {
        let estimator = DefaultSwapAmountEstimator::with_balances();
        let tokens = create_test_tokens();

        let balance_value = BigInt::from(20000u64);
        let mut balances = HashMap::new();
        balances.insert(tokens[0].clone(), Bytes::from(balance_value.to_bytes_be().1));
        balances.insert(tokens[1].clone(), Bytes::from(balance_value.to_bytes_be().1));

        let metadata = create_metadata_with_balances(balances);

        let result = estimator
            .estimate_swap_amounts(&metadata, &tokens)
            .unwrap();

        assert_eq!(result.len(), 2); // 2 pairs: token0->token1, token1->token0

        let amounts01 = result
            .get(&(tokens[0].clone(), tokens[1].clone()))
            .unwrap();
        assert_eq!(amounts01.len(), 4);
        assert_eq!(amounts01[0], Bytes::from(BigInt::from(200u64).to_bytes_be().1)); // 1%
        assert_eq!(amounts01[1], Bytes::from(BigInt::from(400u64).to_bytes_be().1)); // 2%
        assert_eq!(amounts01[2], Bytes::from(BigInt::from(1000u64).to_bytes_be().1)); // 5%
        assert_eq!(amounts01[3], Bytes::from(BigInt::from(2000u64).to_bytes_be().1)); // 10%

        let amounts10 = result
            .get(&(tokens[1].clone(), tokens[0].clone()))
            .unwrap();
        assert_eq!(amounts10.len(), 4);
        assert_eq!(amounts10[0], Bytes::from(BigInt::from(200u64).to_bytes_be().1)); // 1%
        assert_eq!(amounts10[1], Bytes::from(BigInt::from(400u64).to_bytes_be().1)); // 2%
        assert_eq!(amounts10[2], Bytes::from(BigInt::from(1000u64).to_bytes_be().1)); // 5%
        assert_eq!(amounts10[3], Bytes::from(BigInt::from(2000u64).to_bytes_be().1));
        // 10%
    }

    #[tokio::test]
    async fn test_estimate_with_no_data() {
        let estimator = DefaultSwapAmountEstimator::with_balances();
        let tokens = create_test_tokens();

        let metadata = ComponentTracingMetadata::new(TxHash::from([0u8; 32]));

        let result = estimator.estimate_swap_amounts(&metadata, &tokens);

        assert!(matches!(result, Err(EntrypointGenerationError::NoDataAvailable(_))));
    }

    fn create_test_component() -> ProtocolComponent {
        use chrono::DateTime;
        use tycho_common::models::{Chain, ChangeType, TxHash};

        ProtocolComponent {
            id: "test_component".to_string(),
            protocol_system: "uniswap_v4_hooks".to_string(),
            protocol_type_name: "pool".to_string(),
            chain: Chain::Ethereum,
            tokens: create_test_tokens(),
            contract_addresses: vec![Address::from([3u8; 20])],
            static_attributes: [
                ("key_lp_fee".to_string(), Bytes::from(3000u32.to_be_bytes())),
                ("tick_spacing".to_string(), Bytes::from(60i32.to_be_bytes())),
            ]
            .into_iter()
            .collect(),
            change: ChangeType::Creation,
            creation_tx: TxHash::from([0u8; 32]),
            created_at: DateTime::from_timestamp(1640995200, 0)
                .unwrap()
                .naive_utc(),
        }
    }

    fn create_test_hook_data() -> HookEntrypointData {
        HookEntrypointData {
            hook_address: Address::from([4u8; 20]),
            component: create_test_component(),
            component_metadata: create_metadata_with_limits(vec![(
                (Address::from([1u8; 20]), Address::from([2u8; 20])),
                (
                    Bytes::from(U256::from(1000u64).to_be_bytes_vec()),
                    Bytes::from(U256::from(1000u64).to_be_bytes_vec()),
                ),
            )]),
            use_balance_overwrites: false, // Default to false for tests
        }
    }

    fn create_test_context() -> HookTracerContext {
        use chrono::DateTime;
        use tycho_common::models::{blockchain::Block, BlockHash, Chain};

        HookTracerContext {
            block: Block {
                number: 1000,
                hash: BlockHash::from([0u8; 32]),
                parent_hash: BlockHash::from([0u8; 32]),
                chain: Chain::Ethereum,
                ts: DateTime::from_timestamp(1640995200, 0)
                    .unwrap()
                    .naive_utc(),
            },
        }
    }

    struct MockEstimator {
        result: Result<HashMap<(Address, Address), Vec<Bytes>>, EntrypointGenerationError>,
    }

    impl SwapAmountEstimator for MockEstimator {
        fn estimate_swap_amounts(
            &self,
            _metadata: &ComponentTracingMetadata,
            _tokens: &[Address],
        ) -> Result<HashMap<(Address, Address), Vec<Bytes>>, EntrypointGenerationError> {
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn test_hook_entrypoint_generator_success() {
        let mut amounts = HashMap::new();
        amounts.insert(
            (Address::from([1u8; 20]), Address::from([2u8; 20])),
            vec![
                Bytes::from(U256::from(100u64).to_be_bytes_vec()),
                Bytes::from(U256::from(200u64).to_be_bytes_vec()),
            ],
        );

        let estimator = MockEstimator { result: Ok(amounts) };
        let mock_detector = MockBalanceSlotDetector::new();
        let mut generator =
            UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
                estimator,
                Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
                mock_detector,
            );

        let config = HookEntrypointConfig {
            max_sample_size: Some(2),
            min_samples: 1,
            router_address: Some(Address::from([5u8; 20])),
            sender: Some(Address::from([6u8; 20])),
            router_code: None,
            pool_manager: Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
        };
        generator.set_config(config);

        let hook_data = create_test_hook_data();
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        match result {
            Ok(entrypoints) => {
                assert!(!entrypoints.is_empty());
                assert_eq!(entrypoints.len(), 2); // 2 amounts
                for entrypoint in entrypoints {
                    assert_eq!(entrypoint.entry_point.signature, "execute(bytes)");
                    assert_eq!(entrypoint.entry_point.target, Address::from([5u8; 20]));
                }
            }
            Err(EntrypointGenerationError::EntrypointGenerationFailed(_)) => {
                // This is expected when router code file doesn't exist
            }
            Err(e) => panic!("Unexpected error: {e:?}"),
        }
    }

    #[tokio::test]
    async fn test_hook_entrypoint_generator_no_amounts() {
        let estimator = MockEstimator {
            result: Err(EntrypointGenerationError::NoDataAvailable("No data".to_string())),
        };
        let mock_detector = MockBalanceSlotDetector::new();
        let generator = UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let hook_data = create_test_hook_data();
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        assert!(matches!(result, Err(EntrypointGenerationError::NoDataAvailable(_))));
    }

    #[tokio::test]
    async fn test_hook_entrypoint_generator_empty_amounts() {
        let estimator = MockEstimator { result: Ok(HashMap::new()) };
        let mock_detector = MockBalanceSlotDetector::new();
        let generator = UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let hook_data = create_test_hook_data();
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        assert!(matches!(result, Err(EntrypointGenerationError::NoDataAvailable(_))));
    }

    #[tokio::test]
    async fn test_token_reordering() {
        // Create tokens in reverse order (token1 < token0)
        let token0 = Address::from([2u8; 20]);
        let token1 = Address::from([1u8; 20]);
        let tokens = vec![token0.clone(), token1.clone()];

        // Create component with reversed tokens
        let mut component = create_test_component();
        component.tokens = tokens;

        // Create amounts for both swap directions
        let mut amounts = HashMap::new();
        amounts.insert(
            (token0.clone(), token1.clone()),
            vec![Bytes::from(U256::from(100u64).to_be_bytes_vec())],
        );
        amounts.insert(
            (token1.clone(), token0.clone()),
            vec![Bytes::from(U256::from(200u64).to_be_bytes_vec())],
        );

        let estimator = MockEstimator { result: Ok(amounts) };
        let mock_detector = MockBalanceSlotDetector::new();
        let generator = UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let hook_data = HookEntrypointData {
            hook_address: Address::from([4u8; 20]),
            component,
            component_metadata: create_metadata_with_limits(vec![]),
            use_balance_overwrites: false, // Default to false for tests
        };
        let context = create_test_context();

        // This should not fail despite tokens being out of order
        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        assert!(result.is_ok());
        let entrypoints = result.unwrap();
        assert_eq!(entrypoints.len(), 2); // One for each swap direction
    }

    #[tokio::test]
    async fn test_swap_direction_determination() {
        // Create properly ordered tokens
        let token0 = Address::from([1u8; 20]);
        let token1 = Address::from([2u8; 20]);

        // Test data for token0 -> token1 swap
        let mut amounts = HashMap::new();
        amounts.insert(
            (token0.clone(), token1.clone()),
            vec![Bytes::from(U256::from(100u64).to_be_bytes_vec())],
        );

        let estimator = MockEstimator { result: Ok(amounts) };
        let mock_detector = MockBalanceSlotDetector::new();
        let generator = UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let mut component = create_test_component();
        component.tokens = vec![token0.clone(), token1.clone()];

        let hook_data = HookEntrypointData {
            hook_address: Address::from([4u8; 20]),
            component,
            component_metadata: create_metadata_with_limits(vec![]),
            use_balance_overwrites: false, // Default to false for tests
        };
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;
        assert!(result.is_ok());

        // For token0 -> token1 swap, zeroForOne should be true
        // We can't directly test the internal state, but we can verify the entrypoint was created
        let entrypoints = result.unwrap();
        assert_eq!(entrypoints.len(), 1);
    }

    #[tokio::test]
    async fn test_storage_slot_overrides_with_detected_slots() {
        // Create test tokens
        let token0 = Address::from([1u8; 20]);
        let token1 = Address::from([2u8; 20]);

        // Create mock detected balance slots
        let mut detected_slots = HashMap::new();
        detected_slots.insert(token0.clone(), Bytes::from([0x12; 32])); // Mock detected slot for token0
        detected_slots.insert(token1.clone(), Bytes::from([0x34; 32])); // Mock detected slot for token1

        // Create mock balance slot detector with detected slots
        let mut mock_detector = MockBalanceSlotDetector::new();
        let token0_clone = token0.clone();
        mock_detector
            .expect_detect_balance_slots()
            .returning(move |tokens, _holder| {
                let mut result = HashMap::new();
                for token in tokens {
                    let storage_addr = token.clone();
                    let slot_bytes = if *token == token0_clone {
                        Bytes::from([0x12; 32]) // Mock detected slot for token0
                    } else {
                        Bytes::from([0x34; 32]) // Mock detected slot for token1
                    };
                    result.insert(token.clone(), Ok((storage_addr, slot_bytes)));
                }
                result
            });

        // Create test swap amounts
        let mut amounts = HashMap::new();
        amounts.insert(
            (token0.clone(), token1.clone()),
            vec![Bytes::from(U256::from(1000u64).to_be_bytes_vec())],
        );

        let estimator = MockEstimator { result: Ok(amounts) };
        let generator = UniswapV4DefaultHookEntrypointGenerator::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let mut hook_data = create_test_hook_data();
        hook_data.use_balance_overwrites = true; // Enable balance overwrites for storage slot override test
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        assert!(result.is_ok());
        let entrypoints = result.unwrap();
        assert_eq!(entrypoints.len(), 1);

        // Verify the entrypoint has storage overrides
        let entrypoint = &entrypoints[0];
        let TracingParams::RPCTracer(rpc_params) = &entrypoint.params;
        let state_overrides = rpc_params
            .state_overrides
            .as_ref()
            .unwrap();

        // Should have overrides for router, pool manager, sender and token_in
        assert_eq!(state_overrides.len(), 4);

        // Check pool manager has storage overrides
        let pool_manager = Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90"));
        let pool_manager_overrides = state_overrides
            .get(&pool_manager)
            .unwrap();

        let token_0_overrides = state_overrides.get(&token0).unwrap();

        if let Some(StorageOverride::Diff(storage_diff)) = &pool_manager_overrides.slots {
            // Should have at least 2 slots: ERC6909 slot + detected slot for token0
            assert_eq!(storage_diff.len(), 1);

            // Slot should have the same amount_in value (1000)
            let expected_amount = Bytes::from(
                U256::from(1000u64)
                    .to_be_bytes::<32>()
                    .as_slice(),
            );
            for value in storage_diff.values() {
                assert_eq!(value, &expected_amount);
            }
        } else {
            panic!("Expected storage diff but found none");
        }

        if let Some(StorageOverride::Diff(storage_diff)) = &token_0_overrides.slots {
            // Verify that the detected slot for token0 is included
            let detected_slot_key = Bytes::from([0x12; 32]);
            assert!(storage_diff.contains_key(&detected_slot_key));
            // Slot should have 2x amount_in value (1000)
            let expected_amount = Bytes::from(
                U256::from(2000u64)
                    .to_be_bytes::<32>()
                    .as_slice(),
            );
            assert_eq!(
                storage_diff
                    .get(&detected_slot_key)
                    .unwrap(),
                &expected_amount
            );
        }
    }

    #[tokio::test]
    async fn test_storage_slot_overrides_with_detection_error() {
        // Create test tokens
        let token0 = Address::from([1u8; 20]);
        let token1 = Address::from([2u8; 20]);

        // Create mock balance slot detector that returns an error
        let mut mock_detector = MockBalanceSlotDetector::new();
        mock_detector
            .expect_detect_balance_slots()
            .returning(|tokens, _holder| {
                let mut result = HashMap::new();
                for token in tokens {
                    result.insert(token.clone(), Err("Detection failed".to_string()));
                }
                result
            });

        // Create test swap amounts
        let mut amounts = HashMap::new();
        amounts.insert(
            (token0.clone(), token1.clone()),
            vec![Bytes::from(U256::from(1000u64).to_be_bytes_vec())],
        );

        let estimator = MockEstimator { result: Ok(amounts) };
        let generator = UniswapV4DefaultHookEntrypointGenerator::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let mut hook_data = create_test_hook_data();
        hook_data.use_balance_overwrites = true; // Enable balance overwrites to test detection error
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        // Should return an error when balance slot detection fails
        assert!(matches!(result, Err(EntrypointGenerationError::NoDataAvailable(_))));
    }

    #[tokio::test]
    async fn test_storage_slot_overrides_only_erc6909() {
        // Create test tokens
        let token0 = Address::from([1u8; 20]);
        let token1 = Address::from([2u8; 20]);

        // Create test swap amounts
        let mut amounts = HashMap::new();
        amounts.insert(
            (token0.clone(), token1.clone()),
            vec![Bytes::from(U256::from(500u64).to_be_bytes_vec())],
        );

        let estimator = MockEstimator { result: Ok(amounts) };

        // No balance slot detector - should only use ERC6909 overwrites
        let mock_detector = MockBalanceSlotDetector::new();
        let generator = UniswapV4DefaultHookEntrypointGenerator::<_, MockBalanceSlotDetector>::new(
            estimator,
            Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90")),
            mock_detector,
        );

        let hook_data = create_test_hook_data();
        let context = create_test_context();

        let result = generator
            .generate_entrypoints(&hook_data, &context)
            .await;

        assert!(result.is_ok());
        let entrypoints = result.unwrap();
        assert_eq!(entrypoints.len(), 1);

        // Verify the entrypoint has storage overrides
        let entrypoint = &entrypoints[0];
        let TracingParams::RPCTracer(rpc_params) = &entrypoint.params;
        let state_overrides = rpc_params
            .state_overrides
            .as_ref()
            .unwrap();

        // Should have overrides for router, pool manager and sender
        assert_eq!(state_overrides.len(), 3);

        // Check pool manager has storage overrides
        let pool_manager = Address::from(hex!("000000000004444c5dc75cB358380D2e3dE08A90"));
        let pool_manager_overrides = state_overrides
            .get(&pool_manager)
            .unwrap();

        if let Some(StorageOverride::Diff(storage_diff)) = &pool_manager_overrides.slots {
            // Should have exactly 1 slot: only ERC6909 slot
            assert_eq!(storage_diff.len(), 1);

            // The slot should have the amount_in value (500)
            let expected_amount = Bytes::from(
                U256::from(500u64)
                    .to_be_bytes::<32>()
                    .as_slice(),
            );
            for value in storage_diff.values() {
                assert_eq!(value, &expected_amount);
            }
        } else {
            panic!("Expected storage diff but found none");
        }
    }
}
