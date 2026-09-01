use std::collections::HashSet;

use alloy::primitives::{aliases::U24, U8};
use tycho_common::Bytes;

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::NON_PLE_ENCODED_PROTOCOLS,
        gas_estimator::estimate_gas_usage,
        group_swaps::{group_swaps, SwapGroup},
        strategy_encoder::strategy_validators::{
            SequentialSwapValidator, SingleSwapValidator, SplitSwapValidator, SwapValidator,
        },
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{get_token_position, percentage_to_uint24, ple_encode},
    },
    models::{EncodedSolution, EncodingContext, Solution, Strategy, UserTransferType},
    strategy_encoder::StrategyEncoder,
    swap_encoder::SwapEncoder,
};

/// Marker placed in a hop's executor slot when the hop carries a fallback bundle. Must match
/// `LibSwap.FALLBACK_MARKER` in the router contracts.
const FALLBACK_MARKER: [u8; 20] = [0u8; 20];

/// Encodes one router hop: `executor || protocolData` for a plain hop, or — when the hop
/// carries a fallback — the bundle
/// `FALLBACK_MARKER || uint16 primaryLength || primary || fallbackExecutor || fallbackData`,
/// where primary is `executor || protocolData` and primaryLength is its byte length.
fn encode_hop(
    executor_address: &Bytes,
    protocol_data: Vec<u8>,
    fallback: Option<(Bytes, Vec<u8>)>,
) -> Result<Vec<u8>, EncodingError> {
    let mut primary = executor_address.to_vec();
    primary.extend(protocol_data);
    let Some((fallback_executor, fallback_data)) = fallback else {
        return Ok(primary);
    };
    let primary_length = u16::try_from(primary.len()).map_err(|_| {
        EncodingError::InvalidInput(format!(
            "Primary swap data is {} bytes, exceeding the uint16 length prefix",
            primary.len()
        ))
    })?;
    let mut encoded = FALLBACK_MARKER.to_vec();
    encoded.extend(primary_length.to_be_bytes());
    encoded.extend(primary);
    encoded.extend(fallback_executor.to_vec());
    encoded.extend(fallback_data);
    Ok(encoded)
}

/// Encodes a grouped swap's fallback swap, if any, as (executor address, protocol data).
///
/// The fallback must swap the same token pair as the primary, carry no split, and not nest
/// another fallback.
fn encode_fallback(
    strategy: &dyn StrategyEncoder,
    grouped_swap: &SwapGroup,
    router_address: &Bytes,
) -> Result<Option<(Bytes, Vec<u8>)>, EncodingError> {
    let Some(fallback_swap) = grouped_swap
        .swaps
        .first()
        .and_then(|swap| swap.fallback_swap())
    else {
        return Ok(None);
    };
    if grouped_swap.swaps.len() != 1 {
        return Err(EncodingError::FatalError(
            "A swap with a fallback must not be grouped with other swaps".to_string(),
        ));
    }
    if fallback_swap.token_in().address != grouped_swap.token_in ||
        fallback_swap.token_out().address != grouped_swap.token_out
    {
        return Err(EncodingError::InvalidInput(
            "A fallback swap must swap the same token pair as its primary swap".to_string(),
        ));
    }
    if fallback_swap.split() != 0.0 {
        return Err(EncodingError::InvalidInput(
            "A fallback swap must not have a split".to_string(),
        ));
    }
    if fallback_swap.fallback_swap().is_some() {
        return Err(EncodingError::InvalidInput(
            "A fallback swap must not have a fallback itself".to_string(),
        ));
    }

    let protocol = &fallback_swap
        .component()
        .protocol_system;
    let swap_encoder = strategy
        .get_swap_encoder(protocol)
        .ok_or_else(|| {
            EncodingError::InvalidInput(format!("Swap encoder not found for protocol: {protocol}"))
        })?;
    let encoding_context = EncodingContext {
        router_address: Some(router_address.clone()),
        group_token_in: grouped_swap.token_in.clone(),
        group_token_out: grouped_swap.token_out.clone(),
    };
    let protocol_data = swap_encoder.encode_swap(fallback_swap, &encoding_context)?;
    Ok(Some((swap_encoder.executor_address().clone(), protocol_data)))
}

/// Represents the encoder for a swap strategy which supports single swaps.
///
/// # Fields
/// * `swap_encoder_registry`: SwapEncoderRegistry, containing all possible swap encoders
/// * `router_address`: Address of the router to be used to execute swaps
/// * `single_swap_validator`: SingleSwapValidator, responsible for checking validity of the swap
///   path
#[derive(Clone)]
pub(crate) struct SingleSwapStrategyEncoder {
    swap_encoder_registry: SwapEncoderRegistry,
    router_address: Bytes,
    single_swap_validator: SingleSwapValidator,
}

impl SingleSwapStrategyEncoder {
    pub(crate) fn new(
        swap_encoder_registry: SwapEncoderRegistry,
        router_address: Bytes,
    ) -> Result<Self, EncodingError> {
        Ok(Self {
            swap_encoder_registry,
            router_address: router_address.clone(),
            single_swap_validator: SingleSwapValidator,
        })
    }
}

impl StrategyEncoder for SingleSwapStrategyEncoder {
    fn encode_strategy(&self, solution: &Solution) -> Result<EncodedSolution, EncodingError> {
        let function_signature = match solution.user_transfer_type() {
            UserTransferType::TransferFromPermit2 => {
                "singleSwapPermit2(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),((address,uint160,uint48,uint48),address,uint256),bytes,bytes)"
            }
            UserTransferType::TransferFrom => {
                "singleSwap(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
            UserTransferType::UseVaultsFunds => {
                "singleSwapUsingVault(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
        }
        .to_string();
        self.single_swap_validator
            .validate_swap_path(solution.swaps(), solution.token_in(), solution.token_out())?;

        let grouped_swaps = group_swaps(solution.swaps());
        let number_of_groups = grouped_swaps.len();
        if number_of_groups != 1 {
            return Err(EncodingError::InvalidInput(format!(
                "Single strategy only supports exactly one swap for non-groupable protocols. Found {number_of_groups}",
            )));
        }

        let grouped_swap = grouped_swaps
            .first()
            .ok_or_else(|| EncodingError::FatalError("Swap grouping failed".to_string()))?;

        if grouped_swap.split != 0f64 {
            return Err(EncodingError::InvalidInput(
                "Splits not supported for single swaps.".to_string(),
            ));
        }

        let protocol = &grouped_swap.protocol_system;
        let swap_encoder = self
            .get_swap_encoder(protocol)
            .ok_or_else(|| {
                EncodingError::InvalidInput(format!(
                    "Swap encoder not found for protocol: {protocol}"
                ))
            })?;

        let encoding_context = EncodingContext {
            router_address: Some(self.router_address.clone()),
            group_token_in: grouped_swap.token_in.clone(),
            group_token_out: grouped_swap.token_out.clone(),
        };

        let mut grouped_protocol_data: Vec<Vec<u8>> = vec![];
        let mut initial_protocol_data: Vec<u8> = vec![];
        for swap in grouped_swap.swaps.iter() {
            let protocol_data = swap_encoder.encode_swap(swap, &encoding_context)?;
            if encoding_context.group_token_in == *swap.token_in().address {
                initial_protocol_data = protocol_data;
            } else {
                grouped_protocol_data.push(protocol_data);
            }
        }

        if !grouped_protocol_data.is_empty() {
            if NON_PLE_ENCODED_PROTOCOLS.contains(grouped_swap.protocol_system.as_str()) {
                for protocol_data in grouped_protocol_data {
                    initial_protocol_data.extend(protocol_data);
                }
            } else {
                initial_protocol_data.extend(ple_encode(grouped_protocol_data));
            }
        }

        let fallback = encode_fallback(self, grouped_swap, &self.router_address)?;
        let swap_data =
            encode_hop(swap_encoder.executor_address(), initial_protocol_data, fallback)?;
        let gas_usage = estimate_gas_usage(solution, Strategy::Single);
        Ok(EncodedSolution::new(
            swap_data,
            self.router_address.clone(),
            function_signature,
            0,
            gas_usage,
        ))
    }

    fn get_swap_encoder(&self, protocol_system: &str) -> Option<&Box<dyn SwapEncoder>> {
        self.swap_encoder_registry
            .get_encoder(protocol_system)
    }
}

/// Represents the encoder for a swap strategy which supports sequential swaps.
///
/// # Fields
/// * `swap_encoder_registry`: SwapEncoderRegistry, containing all possible swap encoders
/// * `function_signature`: String, the signature for the swap function in the router contract
/// * `native_address`: Address of the chain's native token
/// * `wrapped_address`: Address of the chain's wrapped token
/// * `router_address`: Address of the router to be used to execute swaps
/// * `sequential_swap_validator`: SequentialSwapValidator, responsible for checking validity of
///   sequential swap solutions
#[derive(Clone)]
pub(crate) struct SequentialSwapStrategyEncoder {
    swap_encoder_registry: SwapEncoderRegistry,
    router_address: Bytes,
    sequential_swap_validator: SequentialSwapValidator,
}

impl SequentialSwapStrategyEncoder {
    pub(crate) fn new(
        swap_encoder_registry: SwapEncoderRegistry,
        router_address: Bytes,
    ) -> Result<Self, EncodingError> {
        Ok(Self {
            swap_encoder_registry,
            router_address: router_address.clone(),
            sequential_swap_validator: SequentialSwapValidator,
        })
    }
}

impl StrategyEncoder for SequentialSwapStrategyEncoder {
    fn encode_strategy(&self, solution: &Solution) -> Result<EncodedSolution, EncodingError> {
        let function_signature = match solution.user_transfer_type() {
            UserTransferType::TransferFromPermit2 => {
                "sequentialSwapPermit2(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),((address,uint160,uint48,uint48),address,uint256),bytes,bytes)"
            }
            UserTransferType::TransferFrom => {
                "sequentialSwap(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
            UserTransferType::UseVaultsFunds => {
                "sequentialSwapUsingVault(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
        }
        .to_string();
        self.sequential_swap_validator
            .validate_swap_path(solution.swaps(), solution.token_in(), solution.token_out())?;

        let grouped_swaps = group_swaps(solution.swaps());

        let mut swaps = vec![];
        for grouped_swap in grouped_swaps.iter() {
            let protocol = &grouped_swap.protocol_system;
            let swap_encoder = self
                .get_swap_encoder(protocol)
                .ok_or_else(|| {
                    EncodingError::InvalidInput(format!(
                        "Swap encoder not found for protocol: {protocol}",
                    ))
                })?;

            let encoding_context = EncodingContext {
                router_address: Some(self.router_address.clone()),
                group_token_in: grouped_swap.token_in.clone(),
                group_token_out: grouped_swap.token_out.clone(),
            };

            let mut grouped_protocol_data: Vec<Vec<u8>> = vec![];
            let mut initial_protocol_data: Vec<u8> = vec![];
            for swap in grouped_swap.swaps.iter() {
                let protocol_data = swap_encoder.encode_swap(swap, &encoding_context)?;
                if encoding_context.group_token_in == *swap.token_in().address {
                    initial_protocol_data = protocol_data;
                } else {
                    grouped_protocol_data.push(protocol_data);
                }
            }

            if !grouped_protocol_data.is_empty() {
                if NON_PLE_ENCODED_PROTOCOLS.contains(grouped_swap.protocol_system.as_str()) {
                    for protocol_data in grouped_protocol_data {
                        initial_protocol_data.extend(protocol_data);
                    }
                } else {
                    initial_protocol_data.extend(ple_encode(grouped_protocol_data));
                }
            }

            let fallback = encode_fallback(self, grouped_swap, &self.router_address)?;
            let swap_data =
                encode_hop(swap_encoder.executor_address(), initial_protocol_data, fallback)?;
            swaps.push(swap_data);
        }

        let encoded_swaps = ple_encode(swaps);
        let gas_usage = estimate_gas_usage(solution, Strategy::Sequential);
        Ok(EncodedSolution::new(
            encoded_swaps,
            self.router_address.clone(),
            function_signature,
            0,
            gas_usage,
        ))
    }

    fn get_swap_encoder(&self, protocol_system: &str) -> Option<&Box<dyn SwapEncoder>> {
        self.swap_encoder_registry
            .get_encoder(protocol_system)
    }
}

/// Represents the encoder for a swap strategy which supports split swaps.
///
/// # Fields
/// * `swap_encoder_registry`: SwapEncoderRegistry, containing all possible swap encoders
/// * `native_address`: Address of the chain's native token
/// * `wrapped_address`: Address of the chain's wrapped token
/// * `split_swap_validator`: SplitSwapValidator, responsible for checking validity of split swap
///   solutions
/// * `router_address`: Address of the router to be used to execute swaps
#[derive(Clone)]
pub(crate) struct SplitSwapStrategyEncoder {
    swap_encoder_registry: SwapEncoderRegistry,
    split_swap_validator: SplitSwapValidator,
    router_address: Bytes,
}

impl SplitSwapStrategyEncoder {
    pub(crate) fn new(
        swap_encoder_registry: SwapEncoderRegistry,
        router_address: Bytes,
    ) -> Result<Self, EncodingError> {
        Ok(Self {
            swap_encoder_registry,
            split_swap_validator: SplitSwapValidator,
            router_address: router_address.clone(),
        })
    }

    /// Encodes information necessary for performing a single hop against a given executor for
    /// a protocol as part of a split swap solution. `hop_data` is the pre-encoded
    /// executor + protocol data pair (or fallback bundle) produced by `encode_hop`.
    fn encode_swap_header(
        &self,
        token_in: U8,
        token_out: U8,
        split: U24,
        hop_data: Vec<u8>,
    ) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.push(token_in.to_be_bytes_vec()[0]);
        encoded.push(token_out.to_be_bytes_vec()[0]);
        encoded.extend_from_slice(&split.to_be_bytes_vec());
        encoded.extend(hop_data);
        encoded
    }
}

impl StrategyEncoder for SplitSwapStrategyEncoder {
    fn encode_strategy(&self, solution: &Solution) -> Result<EncodedSolution, EncodingError> {
        let function_signature = match solution.user_transfer_type() {
            UserTransferType::TransferFromPermit2 => {
                "splitSwapPermit2(uint256,address,address,uint256,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),((address,uint160,uint48,uint48),address,uint256),bytes,bytes)"
            }
            UserTransferType::TransferFrom => {
                "splitSwap(uint256,address,address,uint256,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
            UserTransferType::UseVaultsFunds => {
                "splitSwapUsingVault(uint256,address,address,uint256,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            }
        }
        .to_string();
        self.split_swap_validator
            .validate_split_percentages(solution.swaps())?;
        self.split_swap_validator
            .validate_swap_path(solution.swaps(), solution.token_in(), solution.token_out())?;

        // The tokens array is composed of the given token, the checked token and all the
        // intermediary tokens in between. The contract expects the tokens to be in this order.
        let solution_tokens: HashSet<&Bytes> = vec![solution.token_in(), solution.token_out()]
            .into_iter()
            .collect();

        let grouped_swaps = group_swaps(solution.swaps());

        let intermediary_tokens: HashSet<&Bytes> = grouped_swaps
            .iter()
            .flat_map(|grouped_swap| vec![&grouped_swap.token_in, &grouped_swap.token_out])
            .collect();
        let mut intermediary_tokens: Vec<&Bytes> = intermediary_tokens
            .difference(&solution_tokens)
            .cloned()
            .collect();
        // this is only to make the test deterministic (same index for the same token for different
        // runs)
        intermediary_tokens.sort();

        let mut tokens = Vec::with_capacity(2 + intermediary_tokens.len());
        tokens.push(solution.token_in());
        tokens.extend(intermediary_tokens);
        tokens.push(solution.token_out());

        let mut swaps = vec![];
        for grouped_swap in grouped_swaps.iter() {
            let protocol = &grouped_swap.protocol_system;
            let swap_encoder = self
                .get_swap_encoder(protocol)
                .ok_or_else(|| {
                    EncodingError::InvalidInput(format!(
                        "Swap encoder not found for protocol: {protocol}",
                    ))
                })?;

            let encoding_context = EncodingContext {
                router_address: Some(self.router_address.clone()),
                group_token_in: grouped_swap.token_in.clone(),
                group_token_out: grouped_swap.token_out.clone(),
            };

            let mut grouped_protocol_data: Vec<Vec<u8>> = vec![];
            let mut initial_protocol_data: Vec<u8> = vec![];
            for swap in grouped_swap.swaps.iter() {
                let protocol_data = swap_encoder.encode_swap(swap, &encoding_context)?;
                if encoding_context.group_token_in == *swap.token_in().address {
                    initial_protocol_data = protocol_data;
                } else {
                    grouped_protocol_data.push(protocol_data);
                }
            }

            if !grouped_protocol_data.is_empty() {
                if NON_PLE_ENCODED_PROTOCOLS.contains(grouped_swap.protocol_system.as_str()) {
                    for protocol_data in grouped_protocol_data {
                        initial_protocol_data.extend(protocol_data);
                    }
                } else {
                    initial_protocol_data.extend(ple_encode(grouped_protocol_data));
                }
            }

            let fallback = encode_fallback(self, grouped_swap, &self.router_address)?;
            let hop_data =
                encode_hop(swap_encoder.executor_address(), initial_protocol_data, fallback)?;
            let swap_data = self.encode_swap_header(
                get_token_position(&tokens, &grouped_swap.token_in)?,
                get_token_position(&tokens, &grouped_swap.token_out)?,
                percentage_to_uint24(grouped_swap.split),
                hop_data,
            );
            swaps.push(swap_data);
        }

        let encoded_swaps = ple_encode(swaps);
        let tokens_len = if solution.token_in() == solution.token_out() {
            tokens.len() - 1
        } else {
            tokens.len()
        };
        let gas_usage = estimate_gas_usage(solution, Strategy::Split);
        Ok(EncodedSolution::new(
            encoded_swaps,
            self.router_address.clone(),
            function_signature,
            tokens_len,
            gas_usage,
        ))
    }

    fn get_swap_encoder(&self, protocol_system: &str) -> Option<&Box<dyn SwapEncoder>> {
        self.swap_encoder_registry
            .get_encoder(protocol_system)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, str::FromStr};

    use alloy::{hex::encode, primitives::hex};
    use num_bigint::{BigInt, BigUint};
    use tycho_common::{
        models::{protocol::ProtocolComponent, Chain},
        Bytes,
    };

    use super::*;

    fn eth_chain() -> Chain {
        Chain::Ethereum
    }

    fn weth() -> Bytes {
        Bytes::from(hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").to_vec())
    }

    fn get_swap_encoder_registry() -> SwapEncoderRegistry {
        let executors_addresses =
            fs::read_to_string("config/test_executor_addresses.json").unwrap();
        let eth_chain = eth_chain();
        let registry = SwapEncoderRegistry::new(eth_chain);
        registry
            .add_default_encoders(Some(executors_addresses))
            .unwrap()
    }

    fn router_address() -> Bytes {
        Bytes::from_str("0xcd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2").unwrap()
    }

    mod single {
        use super::*;
        use crate::encoding::models::{default_token, Swap};
        #[test]
        fn test_single_swap_strategy_encoder() {
            // Performs a single swap from WETH to DAI on a USV2 pool, with no grouping
            // optimizations.
            let expected_amount_out = BigUint::from_str("2018817438608734439720").unwrap();
            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            );
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SingleSwapStrategyEncoder::new(swap_encoder_registry, router_address()).unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                dai,
                BigUint::from_str("1_000000000000000000").unwrap(),
                expected_amount_out.clone(),
                // 2% below the quote
                &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64),
                vec![swap],
            )
            .with_user_transfer_type(UserTransferType::TransferFromPermit2);

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let expected_swap = String::from(concat!(
                // Swap data
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "a478c2975ab1ea89e8196811f51a7b7ade33eb11", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
            ));
            let hex_calldata = encode(encoded_solution.swaps());

            assert_eq!(hex_calldata, expected_swap);
            assert_eq!(
                encoded_solution.function_signature(),
                "singleSwapPermit2(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),((address,uint160,uint48,uint48),address,uint256),bytes,bytes)"
            );
            assert_eq!(encoded_solution.interacting_with(), &router_address());
        }

        #[test]
        fn test_single_swap_strategy_encoder_with_fallback() {
            // A single WETH -> DAI swap on a USV2 pool carrying a USV3 fallback for the
            // same pair. The hop encodes as a fallback bundle behind the zero-address
            // marker.
            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

            let fallback_swap = Swap::new(
                ProtocolComponent {
                    id: "0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8".to_string(),
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            );
            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_fallback_swap(fallback_swap);

            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SingleSwapStrategyEncoder::new(swap_encoder_registry, router_address()).unwrap();
            let expected_amount_out = BigUint::from_str("2018817438608734439720").unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                dai,
                BigUint::from_str("1_000000000000000000").unwrap(),
                expected_amount_out.clone(),
                &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64),
                vec![swap],
            );

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let expected_swap = String::from(concat!(
                "0000000000000000000000000000000000000000", // fallback marker
                "0050",                                     // primary length (80 bytes)
                // primary: USV2
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "a478c2975ab1ea89e8196811f51a7b7ade33eb11", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
                // fallback: USV3
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
                "000bb8",                                   // pool fee
                "c2e9f25be6257c210d7adf0d4cd6e3e881ba25f8", // component id
                "00",                                       // zero2one
            ));
            assert_eq!(encode(encoded_solution.swaps()), expected_swap);
        }

        #[test]
        fn test_single_swap_strategy_encoder_fallback_token_mismatch() {
            // A fallback swapping a different token pair than its primary is rejected.
            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

            let fallback_swap = Swap::new(
                ProtocolComponent {
                    id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            );
            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_fallback_swap(fallback_swap);

            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SingleSwapStrategyEncoder::new(swap_encoder_registry, router_address()).unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                dai,
                BigUint::from_str("1_000000000000000000").unwrap(),
                BigUint::from_str("2018817438608734439720").unwrap(),
                BigUint::from_str("1978441089836559750925").unwrap(),
                vec![swap],
            );

            let result = encoder.encode_strategy(&solution);
            assert!(matches!(result, Err(EncodingError::InvalidInput(_))));
        }
    }

    mod sequential {
        use super::*;
        use crate::encoding::models::{default_token, Swap};

        #[test]
        fn test_sequential_swap_strategy_encoder_no_permit2() {
            // Performs a sequential swap from WETH to USDC though WBTC using USV2 pools
            //
            //   WETH ───(USV2)──> WBTC ───(USV2)──> USDC

            let weth = weth();
            let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

            let swap_weth_wbtc = Swap::new(
                ProtocolComponent {
                    id: "0xBb2b8038a1640196FbE3e38816F3e67Cba72D940".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(wbtc.clone()),
                BigUint::ZERO,
            );
            let swap_wbtc_usdc = Swap::new(
                ProtocolComponent {
                    id: "0x004375Dff511095CC5A197A54140a24eFEF3A416".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(wbtc.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            );
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SequentialSwapStrategyEncoder::new(swap_encoder_registry, router_address())
                    .unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                usdc,
                BigUint::from_str("1_000000000000000000").unwrap(),
                BigUint::from_str("26173932").unwrap(),
                BigUint::from_str("25650453").unwrap(),
                vec![swap_weth_wbtc, swap_wbtc_usdc],
            );

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let hex_calldata = encode(encoded_solution.swaps());

            let expected = String::from(concat!(
                // swap 1: WETH -> WBTC
                "0050",                                     // swap length (80 bytes)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "bb2b8038a1640196fbe3e38816f3e67cba72d940", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599", // tokenOut (WBTC)
                // swap 2: WBTC -> USDC
                "0050",                                     // swap length (80 bytes)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "004375dff511095cc5a197a54140a24efef3a416", // component id (pool address)
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599", // tokenIn (WBTC)
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // tokenOut (USDC)
            ));

            assert_eq!(hex_calldata, expected);
            assert_eq!(
                encoded_solution.function_signature(),
                "sequentialSwap(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            );
            assert_eq!(encoded_solution.interacting_with(), &router_address());
        }

        #[test]
        fn test_sequential_swap_strategy_encoder_with_fallback() {
            // WETH -> WBTC -> USDC on USV2 pools, where the second hop carries a USV3
            // fallback for the same pair. Only the second hop encodes as a fallback
            // bundle; the first hop keeps the plain layout.
            let weth = weth();
            let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

            let swap_weth_wbtc = Swap::new(
                ProtocolComponent {
                    id: "0xBb2b8038a1640196FbE3e38816F3e67Cba72D940".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(wbtc.clone()),
                BigUint::ZERO,
            );
            let fallback_wbtc_usdc = Swap::new(
                ProtocolComponent {
                    id: "0x99ac8cA7087fA4A2A1FB6357269965A2014ABc35".to_string(),
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(wbtc.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            );
            let swap_wbtc_usdc = Swap::new(
                ProtocolComponent {
                    id: "0x004375Dff511095CC5A197A54140a24eFEF3A416".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(wbtc.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            )
            .with_fallback_swap(fallback_wbtc_usdc);

            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SequentialSwapStrategyEncoder::new(swap_encoder_registry, router_address())
                    .unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                usdc,
                BigUint::from_str("1_000000000000000000").unwrap(),
                BigUint::from_str("26173932").unwrap(),
                BigUint::from_str("25650453").unwrap(),
                vec![swap_weth_wbtc, swap_wbtc_usdc],
            );

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let expected = String::from(concat!(
                // swap 1: WETH -> WBTC, plain hop
                "0050",                                     // swap length (80 bytes)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "bb2b8038a1640196fbe3e38816f3e67cba72d940", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599", // tokenOut (WBTC)
                // swap 2: WBTC -> USDC, fallback bundle
                "00ba",                                     // swap length (186 bytes)
                "0000000000000000000000000000000000000000", // fallback marker
                "0050",                                     // primary length (80 bytes)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "004375dff511095cc5a197a54140a24efef3a416", // component id (pool address)
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599", // tokenIn (WBTC)
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // tokenOut (USDC)
                // fallback: USV3
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599", // tokenIn (WBTC)
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // tokenOut (USDC)
                "000bb8",                                   // pool fee
                "99ac8ca7087fa4a2a1fb6357269965a2014abc35", // component id
                "01",                                       // zero2one
            ));

            assert_eq!(encode(encoded_solution.swaps()), expected);
        }
    }

    mod split {
        use super::*;
        use crate::encoding::models::{default_token, Swap};

        #[test]
        fn test_split_swap_strategy_encoder_with_fallback() {
            // WETH -> DAI split over two USV2 pools (60/40), where the second hop
            // carries a USV3 fallback for the same pair.
            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

            let swap_pool1 = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.6f64);
            let fallback = Swap::new(
                ProtocolComponent {
                    id: "0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8".to_string(),
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            );
            let swap_pool2 = Swap::new(
                ProtocolComponent {
                    id: "0xC3D03e4F041Fd4cD388c549Ee2A29a9E5075882f".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_fallback_swap(fallback);

            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder =
                SplitSwapStrategyEncoder::new(swap_encoder_registry, router_address()).unwrap();
            let expected_amount_out = BigUint::from_str("2018817438608734439720").unwrap();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth,
                dai,
                BigUint::from_str("1_000000000000000000").unwrap(),
                expected_amount_out.clone(),
                &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64),
                vec![swap_pool1, swap_pool2],
            );

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let expected_swaps = [
                // hop 1: plain USV2, 60%
                "0055",                                     // ple encoded swaps (85 bytes)
                "00",                                       // token in index
                "01",                                       // token out index
                "999999",                                   // split (60%)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "a478c2975ab1ea89e8196811f51a7b7ade33eb11", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
                // hop 2: fallback bundle, remaining 40%
                "00bf",                                     // ple encoded swaps (191 bytes)
                "00",                                       // token in index
                "01",                                       // token out index
                "000000",                                   // split (remainder)
                "0000000000000000000000000000000000000000", // fallback marker
                "0050",                                     // primary length (80 bytes)
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "c3d03e4f041fd4cd388c549ee2a29a9e5075882f", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
                // fallback: USV3
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut (DAI)
                "000bb8",                                   // pool fee
                "c2e9f25be6257c210d7adf0d4cd6e3e881ba25f8", // component id
                "00",                                       // zero2one
            ]
            .join("");
            assert_eq!(hex::encode(encoded_solution.swaps()), expected_swaps);
        }

        #[test]
        fn test_split_input_cyclic_swap() {
            // This test has start and end tokens that are the same
            // The flow is:
            //            ┌─ (USV3, 60% split) ──> WETH ─┐
            //            │                              │
            // USDC ──────┤                              ├──(USV2)──> USDC
            //            │                              │
            //            └─ (USV3, 40% split) ──> WETH ─┘

            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

            // USDC -> WETH (Pool 1) - 60% of input
            let swap_usdc_weth_pool1 = Swap::new(
                ProtocolComponent {
                    id: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(), /* USDC-WETH USV3
                                                                                   * Pool 1 */
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(usdc.clone()),
                default_token(weth.clone()),
                BigUint::ZERO,
            )
            .with_split(0.6f64);

            // USDC -> WETH (Pool 2) - 40% of input (remaining)
            let swap_usdc_weth_pool2 = Swap::new(
                ProtocolComponent {
                    id: "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8".to_string(), /* USDC-WETH USV3
                                                                                   * Pool 2 */
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(usdc.clone()),
                default_token(weth.clone()),
                BigUint::ZERO,
            );

            // WETH -> USDC (Pool 2)
            let swap_weth_usdc_pool2 = Swap::new(
                ProtocolComponent {
                    id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(), /* USDC-WETH USV2
                                                                                   * Pool 2 */
                    protocol_system: "uniswap_v2".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            );
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder = SplitSwapStrategyEncoder::new(
                swap_encoder_registry,
                Bytes::from("0xcd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2"),
            )
            .unwrap();

            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc.clone(),
                usdc.clone(),
                BigUint::from_str("100000000").unwrap(),
                BigUint::from_str("99574171").unwrap(),
                BigUint::from_str("97582687").unwrap(),
                vec![swap_usdc_weth_pool1, swap_usdc_weth_pool2, swap_weth_usdc_pool2],
            )
            .with_user_transfer_type(UserTransferType::TransferFromPermit2);

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let hex_calldata = hex::encode(encoded_solution.swaps());

            let expected_swaps = [
                "0059",                                     // ple encoded swaps (89 bytes)
                "00",                                       // token in index
                "01",                                       // token out index
                "999999",                                   // split
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out
                "0001f4",                                   // pool fee
                "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640", // component id
                "01",                                       // zero2one
                "0059",                                     // ple encoded swaps (89 bytes)
                "00",                                       // token in index
                "01",                                       // token out index
                "000000",                                   // split
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out
                "000bb8",                                   // pool fee
                "8ad599c3a0ff1de082011efddc58f1908eb6e6d8", // component id
                "01",                                       // zero2one
                "0055",                                     // ple encoded swaps (85 bytes)
                "01",                                       // token in index
                "00",                                       // token out index
                "000000",                                   // split
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address,
                "b4e16d0168e52d35cacd2c6185b44281ec28c9dc", // component id (pool address)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn (WETH)
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // tokenOut (USDC)
            ]
            .join("");
            assert_eq!(hex_calldata, expected_swaps);
            assert_eq!(
                encoded_solution.function_signature(),
                "splitSwapPermit2(uint256,address,address,uint256,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),((address,uint160,uint48,uint48),address,uint256),bytes,bytes)"
            );
            assert_eq!(encoded_solution.interacting_with(), &router_address());
        }

        #[test]
        fn test_split_output_cyclic_swap() {
            // This test has start and end tokens that are the same
            // The flow is:
            //                        ┌─── (USV3, 60% split) ───┐
            //                        │                         │
            // USDC ──(USV2) ── WETH──|                         ├─> USDC
            //                        │                         │
            //                        └─── (USV3, 40% split) ───┘

            let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

            let swap_usdc_weth_v2 = Swap::new(
                ProtocolComponent {
                    id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(), // USDC-WETH USV2
                    protocol_system: "uniswap_v2".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(usdc.clone()),
                default_token(weth.clone()),
                BigUint::ZERO,
            );

            let swap_weth_usdc_v3_pool1 = Swap::new(
                ProtocolComponent {
                    id: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(), /* USDC-WETH USV3
                                                                                   * Pool 1 */
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            )
            .with_split(0.6f64);

            let swap_weth_usdc_v3_pool2 = Swap::new(
                ProtocolComponent {
                    id: "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8".to_string(), /* USDC-WETH USV3
                                                                                   * Pool 1 */
                    protocol_system: "uniswap_v3".to_string(),
                    static_attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "fee".to_string(),
                            Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                        );
                        attrs
                    },
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            );

            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder = SplitSwapStrategyEncoder::new(
                swap_encoder_registry,
                Bytes::from("0xcd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2"),
            )
            .unwrap();

            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc.clone(),
                usdc.clone(),
                BigUint::from_str("100000000").unwrap(),
                BigUint::from_str("99025908").unwrap(),
                BigUint::from_str("97045389").unwrap(),
                vec![swap_usdc_weth_v2, swap_weth_usdc_v3_pool1, swap_weth_usdc_v3_pool2],
            );

            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();

            let hex_calldata = hex::encode(encoded_solution.swaps());

            let expected_swaps = [
                "0055",                                     // ple encoded swaps (85 bytes)
                "00",                                       // token in index
                "01",                                       // token out index
                "000000",                                   // split
                "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
                "b4e16d0168e52d35cacd2c6185b44281ec28c9dc", // component id (pool address)
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in (USDC)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out (WETH)
                "0059",                                     // ple encoded swaps (89 bytes)
                "01",                                       // token in index
                "00",                                       // token out index
                "999999",                                   // split
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token out
                "0001f4",                                   // pool fee
                "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640", // component id
                "00",                                       // zero2one
                "0059",                                     // ple encoded swaps (89 bytes)
                "01",                                       // token in index
                "00",                                       // token out index
                "000000",                                   // split
                "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token out
                "000bb8",                                   // pool fee
                "8ad599c3a0ff1de082011efddc58f1908eb6e6d8", // component id
                "00",                                       // zero2one
            ]
            .join("");

            assert_eq!(hex_calldata, expected_swaps);
            assert_eq!(
                encoded_solution.function_signature(),
                "splitSwap(uint256,address,address,uint256,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)"
            );
            assert_eq!(encoded_solution.interacting_with(), &router_address());
        }
    }
}
