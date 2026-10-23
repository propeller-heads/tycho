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
        subsidy::{select_subsidized_group, SubsidizedGroup, SubsidizedGroupSignature},
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{
            find_ple_item_end, get_token_position, map_on_threads, percentage_to_uint24, ple_encode,
        },
    },
    models::{EncodedSolution, EncodingContext, Solution, Strategy, UserTransferType},
};

/// The protocol data of one swap group and the executor that runs it.
struct EncodedSwapGroup {
    executor_address: Bytes,
    protocol_data: Vec<u8>,
}

/// Encodes the swaps of one group into the protocol data its executor expects.
fn encode_swap_group(
    swap_encoder_registry: &SwapEncoderRegistry,
    grouped_swap: &SwapGroup,
    router_address: &Bytes,
    origin: Option<&Bytes>,
) -> Result<EncodedSwapGroup, EncodingError> {
    let protocol = &grouped_swap.protocol_system;
    let swap_encoder = swap_encoder_registry
        .get_encoder(protocol)
        .ok_or_else(|| {
            EncodingError::InvalidInput(format!("Swap encoder not found for protocol: {protocol}"))
        })?;

    let encoding_context = EncodingContext {
        router_address: Some(router_address.clone()),
        group_token_in: grouped_swap.token_in.clone(),
        group_token_out: grouped_swap.token_out.clone(),
        origin: origin.cloned(),
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
        if NON_PLE_ENCODED_PROTOCOLS.contains(protocol.as_str()) {
            for protocol_data in grouped_protocol_data {
                initial_protocol_data.extend(protocol_data);
            }
        } else {
            initial_protocol_data.extend(ple_encode(grouped_protocol_data));
        }
    }

    Ok(EncodedSwapGroup {
        executor_address: swap_encoder.executor_address().clone(),
        protocol_data: initial_protocol_data,
    })
}

/// Runs the subsidized group through the subsidizing executor.
fn wrap_subsidized_group(
    subsidized_group: Option<&SubsidizedGroup>,
    encoded_groups: &mut [EncodedSwapGroup],
) -> Result<Option<SubsidizedGroupSignature>, EncodingError> {
    let Some(subsidized_group) = subsidized_group else {
        return Ok(None);
    };
    let encoded_group = &mut encoded_groups[subsidized_group.group_index];
    subsidized_group
        .wrap(&mut encoded_group.executor_address, &mut encoded_group.protocol_data)
        .map(Some)
}

/// Encodes every swap group and keeps the input order.
///
/// Groups run through [`map_on_threads`] only when one of their encoders blocks on a quote
/// request; a route of pure on-chain protocols encodes on the calling thread.
fn encode_swap_groups(
    swap_encoder_registry: &SwapEncoderRegistry,
    grouped_swaps: &[SwapGroup],
    router_address: &Bytes,
    origin: Option<&Bytes>,
) -> Result<Vec<EncodedSwapGroup>, EncodingError> {
    let any_group_blocks = grouped_swaps.iter().any(|group| {
        swap_encoder_registry
            .get_encoder(&group.protocol_system)
            .is_some_and(|encoder| encoder.blocks_on_quote())
    });
    if !any_group_blocks {
        let mut encoded_groups = Vec::with_capacity(grouped_swaps.len());
        for grouped_swap in grouped_swaps {
            encoded_groups.push(encode_swap_group(
                swap_encoder_registry,
                grouped_swap,
                router_address,
                origin,
            )?);
        }
        return Ok(encoded_groups);
    }
    map_on_threads(grouped_swaps, |grouped_swap| {
        encode_swap_group(swap_encoder_registry, grouped_swap, router_address, origin)
    })
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

    /// Encodes information necessary for performing a single hop against a given executor for
    /// a protocol.
    fn encode_swap_header(&self, executor_address: Bytes, protocol_data: Vec<u8>) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend(executor_address.to_vec());
        encoded.extend(protocol_data);
        encoded
    }
}

impl SingleSwapStrategyEncoder {
    /// Encodes the solution into calldata for the router's `singleSwap` method family.
    pub(crate) fn encode_strategy(
        &self,
        solution: &Solution,
    ) -> Result<EncodedSolution, EncodingError> {
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

        let subsidized_group =
            select_subsidized_group(solution, &grouped_swaps, &self.swap_encoder_registry)?;
        let mut encoded_group = encode_swap_group(
            &self.swap_encoder_registry,
            grouped_swap,
            &self.router_address,
            solution.origin(),
        )?;
        let subsidized_group_signature = wrap_subsidized_group(
            subsidized_group.as_ref(),
            std::slice::from_mut(&mut encoded_group),
        )?;
        let swap_data =
            self.encode_swap_header(encoded_group.executor_address, encoded_group.protocol_data);
        let subsidy_signing_data = subsidized_group_signature
            .map(|signature| signature.build_signing_data(swap_data.len()));
        let gas_usage = estimate_gas_usage(solution, Strategy::Single);
        Ok(EncodedSolution::new(
            swap_data,
            self.router_address.clone(),
            function_signature,
            0,
            gas_usage,
        )
        .with_subsidy_signing_data(subsidy_signing_data))
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

    /// Encodes information necessary for performing a single hop against a given executor for
    /// a protocol.
    fn encode_swap_header(&self, executor_address: Bytes, protocol_data: Vec<u8>) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend(executor_address.to_vec());
        encoded.extend(protocol_data);
        encoded
    }
}

impl SequentialSwapStrategyEncoder {
    /// Encodes the solution into calldata for the router's `sequentialSwap` method family.
    pub(crate) fn encode_strategy(
        &self,
        solution: &Solution,
    ) -> Result<EncodedSolution, EncodingError> {
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
        let subsidized_group =
            select_subsidized_group(solution, &grouped_swaps, &self.swap_encoder_registry)?;
        let mut encoded_groups = encode_swap_groups(
            &self.swap_encoder_registry,
            &grouped_swaps,
            &self.router_address,
            solution.origin(),
        )?;
        let subsidized_group_signature =
            wrap_subsidized_group(subsidized_group.as_ref(), &mut encoded_groups)?;

        let mut swaps = vec![];
        for encoded_group in encoded_groups {
            let swap_data = self
                .encode_swap_header(encoded_group.executor_address, encoded_group.protocol_data);
            swaps.push(swap_data);
        }

        let subsidy_signing_data = subsidized_group_signature.map(|signature| {
            signature.build_signing_data(find_ple_item_end(&swaps, signature.group_index))
        });
        let encoded_swaps = ple_encode(swaps);
        let gas_usage = estimate_gas_usage(solution, Strategy::Sequential);
        Ok(EncodedSolution::new(
            encoded_swaps,
            self.router_address.clone(),
            function_signature,
            0,
            gas_usage,
        )
        .with_subsidy_signing_data(subsidy_signing_data))
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
    /// a protocol as part of a split swap solution.
    fn encode_swap_header(
        &self,
        token_in: U8,
        token_out: U8,
        split: U24,
        executor_address: Bytes,
        protocol_data: Vec<u8>,
    ) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.push(token_in.to_be_bytes_vec()[0]);
        encoded.push(token_out.to_be_bytes_vec()[0]);
        encoded.extend_from_slice(&split.to_be_bytes_vec());
        encoded.extend(executor_address.to_vec());
        encoded.extend(protocol_data);
        encoded
    }
}

impl SplitSwapStrategyEncoder {
    /// Encodes the solution into calldata for the router's `splitSwap` method family.
    pub(crate) fn encode_strategy(
        &self,
        solution: &Solution,
    ) -> Result<EncodedSolution, EncodingError> {
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

        // Look the token indices up before encoding, so an unknown token fails without an RFQ
        // encoder requesting a signed quote first.
        let mut token_indices = Vec::with_capacity(grouped_swaps.len());
        for grouped_swap in &grouped_swaps {
            token_indices.push((
                get_token_position(&tokens, &grouped_swap.token_in)?,
                get_token_position(&tokens, &grouped_swap.token_out)?,
            ));
        }

        let subsidized_group =
            select_subsidized_group(solution, &grouped_swaps, &self.swap_encoder_registry)?;
        let mut encoded_groups = encode_swap_groups(
            &self.swap_encoder_registry,
            &grouped_swaps,
            &self.router_address,
            solution.origin(),
        )?;
        let subsidized_group_signature =
            wrap_subsidized_group(subsidized_group.as_ref(), &mut encoded_groups)?;

        let mut swaps = Vec::with_capacity(grouped_swaps.len());
        for (index, encoded_group) in encoded_groups.into_iter().enumerate() {
            let (token_in_index, token_out_index) = token_indices[index];
            let swap_data = self.encode_swap_header(
                token_in_index,
                token_out_index,
                percentage_to_uint24(grouped_swaps[index].split),
                encoded_group.executor_address,
                encoded_group.protocol_data,
            );
            swaps.push(swap_data);
        }

        let subsidy_signing_data = subsidized_group_signature.map(|signature| {
            signature.build_signing_data(find_ple_item_end(&swaps, signature.group_index))
        });
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
        )
        .with_subsidy_signing_data(subsidy_signing_data))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        str::FromStr,
        time::{Duration, Instant},
    };

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
        use crate::encoding::{
            evm::testing_utils::recording_bebop_swap,
            models::{default_token, Swap},
        };

        #[test]
        fn test_single_swap_passes_solution_origin_to_rfq_quote() {
            let origin = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
            let received_origins: std::sync::Arc<std::sync::Mutex<Vec<Option<Bytes>>>> =
                Default::default();
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
            let swap = recording_bebop_swap(
                usdc.clone(),
                weth(),
                Duration::ZERO,
                received_origins.clone(),
            );
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc,
                weth(),
                BigUint::from(1_000u64),
                BigUint::from(1_000u64),
                BigUint::from(900u64),
                vec![swap],
            )
            .with_origin(origin.clone());
            let encoder =
                SingleSwapStrategyEncoder::new(get_swap_encoder_registry(), router_address())
                    .unwrap();

            encoder
                .encode_strategy(&solution)
                .unwrap();

            assert_eq!(*received_origins.lock().unwrap(), vec![Some(origin)]);
        }

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
    }

    mod sequential {
        use super::*;
        use crate::encoding::{
            evm::testing_utils::delayed_bebop_swap,
            models::{default_token, Swap},
        };

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

        /// Two RFQ hops each wait 300ms for their quote. Encoded together they finish well
        /// before the 600ms a one-after-the-other encoding needs, and stay in route order.
        #[test]
        fn test_sequential_swap_encodes_rfq_hops_in_parallel() {
            let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
            let weth = weth();
            let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
            let delay = Duration::from_millis(300);
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc.clone(),
                dai.clone(),
                BigUint::from(1_000u64),
                BigUint::from(1_000u64),
                BigUint::from(900u64),
                vec![
                    delayed_bebop_swap(usdc.clone(), weth.clone(), delay),
                    delayed_bebop_swap(weth.clone(), dai.clone(), delay),
                ],
            );
            let encoder =
                SequentialSwapStrategyEncoder::new(get_swap_encoder_registry(), router_address())
                    .unwrap();

            let start = Instant::now();
            let encoded_solution = encoder
                .encode_strategy(&solution)
                .unwrap();
            let elapsed = start.elapsed();

            assert!(elapsed < Duration::from_millis(500), "encoding took {elapsed:?}");
            let hex_calldata = encode(encoded_solution.swaps());
            let first_hop = hex_calldata
                .find(&encode(&usdc)[..])
                .unwrap();
            let second_hop = hex_calldata
                .find(&encode(&dai)[..])
                .unwrap();
            assert!(first_hop < second_hop, "hops are out of route order");
        }
    }

    mod split {
        use super::*;
        use crate::encoding::models::{default_token, Swap};

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

    mod subsidy {
        use alloy::primitives::B256;
        use rstest::rstest;

        use super::*;
        use crate::encoding::{
            errors::EncodingError,
            evm::subsidy::{SUBSIDY_SIGNATURE_LEN, SUBSIDY_SIGNATURE_OFFSET},
            models::{default_token, SubsidyTerms, Swap},
        };

        const EXECUTOR: [u8; 20] = [0x11; 20];
        const PAYER: [u8; 20] = [0x33; 20];
        const SIGNATURE: [u8; SUBSIDY_SIGNATURE_LEN] = [0xab; SUBSIDY_SIGNATURE_LEN];
        const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
        const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        const ETH: &str = "0x0000000000000000000000000000000000000000";
        /// Executor address in front of the protocol data of a sequential swap.
        const SEQUENTIAL_SWAP_HEADER_LEN: usize = 20;
        /// Token indexes, split and executor address in front of a split swap's protocol data.
        const SPLIT_SWAP_HEADER_LEN: usize = 25;

        #[derive(Clone, Copy, Debug)]
        enum StrategyKind {
            Single,
            Sequential,
            Split,
        }

        fn registry() -> SwapEncoderRegistry {
            get_swap_encoder_registry()
                .register_subsidy_contracts(Bytes::from(EXECUTOR), Bytes::from(PAYER))
        }

        fn swap(protocol_system: &str, pool: &str, token_in: &str, token_out: &str) -> Swap {
            component_swap(
                ProtocolComponent {
                    id: pool.to_string(),
                    protocol_system: protocol_system.to_string(),
                    ..Default::default()
                },
                token_in,
                token_out,
            )
        }

        fn component_swap(component: ProtocolComponent, token_in: &str, token_out: &str) -> Swap {
            Swap::new(
                component,
                default_token(Bytes::from_str(token_in).unwrap()),
                default_token(Bytes::from_str(token_out).unwrap()),
                BigUint::ZERO,
            )
        }

        fn native_wrap(token_in: &str, token_out: &str) -> Swap {
            swap("native_wrapper", "", token_in, token_out)
        }

        fn usv2_weth_dai() -> Swap {
            swap(
                "uniswap_v2",
                "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11",
                &weth().to_string(),
                DAI,
            )
        }

        fn sushi_weth_dai() -> Swap {
            swap(
                "sushiswap_v2",
                "0xC3D03e4F041Fd4cD388c549Ee2A29a9E5075882f",
                &weth().to_string(),
                DAI,
            )
        }

        fn usv2_dai_weth() -> Swap {
            swap(
                "uniswap_v2",
                "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11",
                DAI,
                &weth().to_string(),
            )
        }

        fn usv4_usdc_eth() -> Swap {
            let static_attributes = HashMap::from([
                ("key_lp_fee".to_string(), Bytes::from(BigInt::from(3000).to_signed_bytes_be())),
                ("tick_spacing".to_string(), Bytes::from(BigInt::from(60).to_signed_bytes_be())),
            ]);
            component_swap(
                ProtocolComponent {
                    id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d"
                        .to_string(),
                    protocol_system: "uniswap_v4".to_string(),
                    static_attributes,
                    ..Default::default()
                },
                USDC,
                ETH,
            )
        }

        fn unsubsidized_solution(token_in: &str, token_out: &str, swaps: Vec<Swap>) -> Solution {
            Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::from_str(token_in).unwrap(),
                Bytes::from_str(token_out).unwrap(),
                BigUint::from(1_000_000_000_000_000_000u64),
                BigUint::from(1_000u64),
                BigUint::from(900u64),
                swaps,
            )
        }

        fn subsidized_solution(
            token_in: &str,
            token_out: &str,
            swaps: Vec<Swap>,
            subsidized_swap_index: usize,
        ) -> Solution {
            unsubsidized_solution(token_in, token_out, swaps).with_subsidy(SubsidyTerms {
                amount: BigUint::from(100u32),
                min_amount_in: BigUint::from(1u32),
                nonce: B256::with_last_byte(7),
                deadline: 1_700_000_000,
                subsidized_swap_index,
            })
        }

        fn split_weth_to_dai_swaps() -> Vec<Swap> {
            vec![usv2_weth_dai().with_split(0.7), sushi_weth_dai()]
        }

        fn split_weth_to_dai(subsidized_swap_index: usize) -> Solution {
            subsidized_solution(
                &weth().to_string(),
                DAI,
                split_weth_to_dai_swaps(),
                subsidized_swap_index,
            )
        }

        fn encode_kind(
            kind: StrategyKind,
            solution: &Solution,
        ) -> Result<EncodedSolution, EncodingError> {
            match kind {
                StrategyKind::Single => {
                    SingleSwapStrategyEncoder::new(registry(), router_address())?
                        .encode_strategy(solution)
                }
                StrategyKind::Sequential => {
                    SequentialSwapStrategyEncoder::new(registry(), router_address())?
                        .encode_strategy(solution)
                }
                StrategyKind::Split => SplitSwapStrategyEncoder::new(registry(), router_address())?
                    .encode_strategy(solution),
            }
        }

        fn solution_for(kind: StrategyKind) -> Solution {
            match kind {
                StrategyKind::Single => {
                    subsidized_solution(&weth().to_string(), DAI, vec![usv2_weth_dai()], 0)
                }
                StrategyKind::Sequential => subsidized_solution(
                    &weth().to_string(),
                    USDC,
                    vec![
                        usv2_weth_dai(),
                        swap("uniswap_v2", "0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5", DAI, USDC),
                    ],
                    1,
                ),
                // The second swap, so the offset counts the first swap's bytes.
                StrategyKind::Split => split_weth_to_dai(1),
            }
        }

        /// Where the signature starts in the swaps, found from the subsidizing executor's address.
        fn find_signature_in_swaps(swaps: &[u8]) -> usize {
            let executor = swaps
                .windows(EXECUTOR.len())
                .position(|window| window == EXECUTOR)
                .expect("swaps name the subsidizing executor");
            executor + EXECUTOR.len() + SUBSIDY_SIGNATURE_OFFSET
        }

        #[rstest]
        fn test_write_subsidy_signature(
            #[values(StrategyKind::Single, StrategyKind::Sequential, StrategyKind::Split)]
            kind: StrategyKind,
        ) {
            let mut encoded = encode_kind(kind, &solution_for(kind)).unwrap();
            let start = find_signature_in_swaps(encoded.swaps());
            let mut expected = encoded.swaps().to_vec();
            expected[start..start + SIGNATURE.len()].copy_from_slice(&SIGNATURE);

            encoded
                .write_subsidy_signature(&SIGNATURE)
                .unwrap();

            assert_eq!(encoded.subsidy_signature_offset_in_swaps(), Some(start));
            assert_eq!(encoded.swaps(), expected);
        }

        /// The swaps of `ple_encode` output.
        fn decode_ple_items(mut encoded: &[u8]) -> Vec<Vec<u8>> {
            let mut items = vec![];
            while !encoded.is_empty() {
                let len = u16::from_be_bytes([encoded[0], encoded[1]]) as usize;
                items.push(encoded[2..2 + len].to_vec());
                encoded = &encoded[2 + len..];
            }
            items
        }

        /// Asserts that `subsidized` runs `unsubsidized`'s executor and protocol data through the
        /// subsidizing executor, after the same `header_len` bytes.
        fn assert_wraps(subsidized: &[u8], unsubsidized: &[u8], header_len: usize) {
            let inner_executor = SUBSIDY_SIGNATURE_OFFSET + SIGNATURE.len();
            let (header, executor) = unsubsidized[..header_len].split_at(header_len - 20);
            assert_eq!(subsidized[..header_len - 20], *header);
            assert_eq!(subsidized[header_len - 20..header_len], EXECUTOR);
            let wrapper_data = &subsidized[header_len..];
            assert_eq!(wrapper_data[inner_executor..inner_executor + 20], *executor);
            assert_eq!(wrapper_data[inner_executor + 20..], unsubsidized[header_len..]);
        }

        #[rstest]
        #[case::larger_swap(0)]
        #[case::smaller_swap(1)]
        fn test_subsidized_split_swap(#[case] subsidized_swap_index: usize) {
            let unsubsidized = encode_kind(
                StrategyKind::Split,
                &unsubsidized_solution(&weth().to_string(), DAI, split_weth_to_dai_swaps()),
            )
            .unwrap();
            let subsidized =
                encode_kind(StrategyKind::Split, &split_weth_to_dai(subsidized_swap_index))
                    .unwrap();

            let unsubsidized_swaps = decode_ple_items(unsubsidized.swaps());
            let subsidized_swaps = decode_ple_items(subsidized.swaps());
            assert_eq!(subsidized_swaps.len(), 2);
            let other = 1 - subsidized_swap_index;
            assert_eq!(subsidized_swaps[other], unsubsidized_swaps[other]);
            assert_wraps(
                &subsidized_swaps[subsidized_swap_index],
                &unsubsidized_swaps[subsidized_swap_index],
                SPLIT_SWAP_HEADER_LEN,
            );
        }

        #[rstest]
        #[case::final_unwrap(DAI, ETH, vec![usv2_dai_weth(), native_wrap(&weth().to_string(), ETH)])]
        #[case::final_wrap(USDC, &weth().to_string(), vec![usv4_usdc_eth(), native_wrap(ETH, &weth().to_string())])]
        fn test_subsidized_pool_swap_before_final_wrap_or_unwrap(
            #[case] token_in: &str,
            #[case] token_out: &str,
            #[case] swaps: Vec<Swap>,
        ) {
            let unsubsidized = encode_kind(
                StrategyKind::Sequential,
                &unsubsidized_solution(token_in, token_out, swaps.clone()),
            )
            .unwrap();

            let subsidized = encode_kind(
                StrategyKind::Sequential,
                &subsidized_solution(token_in, token_out, swaps, 0),
            )
            .unwrap();

            let unsubsidized_swaps = decode_ple_items(unsubsidized.swaps());
            let subsidized_swaps = decode_ple_items(subsidized.swaps());
            assert_wraps(&subsidized_swaps[0], &unsubsidized_swaps[0], SEQUENTIAL_SWAP_HEADER_LEN);
            assert_eq!(subsidized_swaps[1], unsubsidized_swaps[1]);
        }
    }
}
