use std::collections::HashSet;

use num_bigint::BigUint;
use tycho_common::Bytes;

use crate::encoding::{
    errors::EncodingError,
    evm::{
        group_swaps::group_swaps,
        strategy_encoder::strategy_encoders::{
            SequentialSwapStrategyEncoder, SingleSwapStrategyEncoder, SplitSwapStrategyEncoder,
        },
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::map_on_threads,
    },
    models::{EncodedSolution, Solution},
    tycho_encoder::TychoEncoder,
};

/// Encodes solutions to be used by the TychoRouterV3.
///
/// # Fields
/// * `single_swap_strategy`: Encoder for single swaps
/// * `sequential_swap_strategy`: Encoder for sequential swaps
/// * `split_swap_strategy`: Encoder for split swaps
/// * `swap_encoder_registry`: SwapEncoderRegistry, containing all possible swap encoders
/// * `router_address`: Address of the Tycho router contract
#[derive(Clone)]
pub(crate) struct TychoRouterEncoder {
    single_swap_strategy: SingleSwapStrategyEncoder,
    sequential_swap_strategy: SequentialSwapStrategyEncoder,
    split_swap_strategy: SplitSwapStrategyEncoder,
    swap_encoder_registry: SwapEncoderRegistry,
}

impl TychoRouterEncoder {
    pub(crate) fn new(
        swap_encoder_registry: SwapEncoderRegistry,
        router_address: Bytes,
    ) -> Result<Self, EncodingError> {
        Ok(TychoRouterEncoder {
            single_swap_strategy: SingleSwapStrategyEncoder::new(
                swap_encoder_registry.clone(),
                router_address.clone(),
            )?,
            sequential_swap_strategy: SequentialSwapStrategyEncoder::new(
                swap_encoder_registry.clone(),
                router_address.clone(),
            )?,
            split_swap_strategy: SplitSwapStrategyEncoder::new(
                swap_encoder_registry.clone(),
                router_address.clone(),
            )?,
            swap_encoder_registry,
        })
    }

    /// Whether any swap in the solution encodes through an encoder that blocks on a quote
    /// request.
    fn blocks_on_quote(&self, solution: &Solution) -> bool {
        solution.swaps().iter().any(|swap| {
            self.swap_encoder_registry
                .get_encoder(&swap.component().protocol_system)
                .is_some_and(|encoder| encoder.blocks_on_quote())
        })
    }

    fn encode_solution(&self, solution: &Solution) -> Result<EncodedSolution, EncodingError> {
        self.validate_solution(solution)?;

        let groups = group_swaps(solution.swaps());

        let encoded_solution = if groups.len() == 1 {
            self.single_swap_strategy
                .encode_strategy(solution)?
        } else if solution
            .swaps()
            .iter()
            .all(|swap| swap.split() == 0.0)
        {
            self.sequential_swap_strategy
                .encode_strategy(solution)?
        } else {
            self.split_swap_strategy
                .encode_strategy(solution)?
        };

        Ok(encoded_solution)
    }
}

impl TychoEncoder for TychoRouterEncoder {
    /// Encodes every solution and keeps the input order.
    ///
    /// Solutions run through [`map_on_threads`] only when one of them blocks on a quote
    /// request; a batch of pure on-chain solutions encodes on the calling thread.
    fn encode_solutions(
        &self,
        solutions: Vec<Solution>,
    ) -> Result<Vec<EncodedSolution>, EncodingError> {
        // Validate every solution before encoding any, so an invalid solution fails without the
        // other solutions requesting signed quotes first.
        for solution in &solutions {
            self.validate_solution(solution)?;
        }
        if !solutions
            .iter()
            .any(|solution| self.blocks_on_quote(solution))
        {
            let mut encoded_solutions = Vec::with_capacity(solutions.len());
            for solution in &solutions {
                encoded_solutions.push(self.encode_solution(solution)?);
            }
            return Ok(encoded_solutions);
        }
        map_on_threads(&solutions, |solution| self.encode_solution(solution))
    }

    /// Raises an `EncodingError` if the solution is not considered valid.
    ///
    /// A solution is considered valid if all the following conditions are met:
    /// * The solution has at least one swap.
    /// * The quoted `expected_amount_out` is non-zero (the router rejects a zero
    ///   `expectedAmountOut`).
    /// * The token cannot appear more than once in the solution unless it is the first and last
    ///   token (i.e. a true cyclical swap).
    fn validate_solution(&self, solution: &Solution) -> Result<(), EncodingError> {
        if solution.swaps().is_empty() {
            return Err(EncodingError::FatalError("No swaps found in solution".to_string()));
        }
        if solution.expected_amount_out() == &BigUint::ZERO {
            return Err(EncodingError::FatalError(
                "Solution expected_amount_out must be non-zero: the router rejects a zero \
                 expectedAmountOut"
                    .to_string(),
            ));
        }

        let swaps = solution.swaps();
        let mut solution_tokens = vec![];
        let mut split_tokens_already_considered = HashSet::new();
        for (i, swap) in swaps.iter().enumerate() {
            // so we don't count the split tokens more than once
            if swap.split() != 0.0 {
                if !split_tokens_already_considered.contains(&swap.token_in().address) {
                    solution_tokens.push(&swap.token_in().address);
                    split_tokens_already_considered.insert(&swap.token_in().address);
                }
            } else {
                // it might be the last swap of the split or a regular swap
                if !split_tokens_already_considered.contains(&swap.token_in().address) {
                    solution_tokens.push(&swap.token_in().address);
                }
            }
            if i == swaps.len() - 1 {
                solution_tokens.push(&swap.token_out().address);
            }
        }

        if solution_tokens.len() !=
            solution_tokens
                .iter()
                .cloned()
                .collect::<HashSet<&Bytes>>()
                .len()
        {
            if let Some(last_swap) = swaps.last() {
                if *swaps[0].token_in().address != *last_swap.token_out().address {
                    return Err(EncodingError::FatalError(
                        "Cyclical swaps are only allowed if they are the first and last token of a solution".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, str::FromStr};

    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use tycho_common::models::{protocol::ProtocolComponent, Chain};

    use super::*;
    use crate::encoding::models::{default_token, Swap};

    fn dai() -> Bytes {
        Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap()
    }

    fn eth() -> Bytes {
        Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap()
    }

    fn weth() -> Bytes {
        Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
    }

    fn usdc() -> Bytes {
        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()
    }

    fn wbtc() -> Bytes {
        Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap()
    }

    // Fee and tick spacing information for this test is obtained by querying the
    // USV4 Position Manager contract: 0xbd216513d74c8cf14cf4747e6aaa6420ff64ee9e
    // Using the poolKeys function with the first 25 bytes of the pool id
    fn swap_usdc_eth_univ4() -> Swap {
        let pool_fee_usdc_eth = Bytes::from(BigInt::from(3000).to_signed_bytes_be());
        let tick_spacing_usdc_eth = Bytes::from(BigInt::from(60).to_signed_bytes_be());
        let mut static_attributes_usdc_eth: HashMap<String, Bytes> = HashMap::new();
        static_attributes_usdc_eth.insert("key_lp_fee".into(), pool_fee_usdc_eth);
        static_attributes_usdc_eth.insert("tick_spacing".into(), tick_spacing_usdc_eth);
        Swap::new(
            ProtocolComponent {
                id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d"
                    .to_string(),
                protocol_system: "uniswap_v4".to_string(),
                static_attributes: static_attributes_usdc_eth,
                ..Default::default()
            },
            default_token(usdc().clone()),
            default_token(eth().clone()),
            BigUint::ZERO,
        )
    }

    fn router_address() -> Bytes {
        Bytes::from_str("0x6bc529DC7B81A031828dDCE2BC419d01FF268C66").unwrap()
    }

    fn eth_chain() -> Chain {
        Chain::Ethereum
    }

    fn get_swap_encoder_registry() -> SwapEncoderRegistry {
        let executors_addresses =
            fs::read_to_string("config/test_executor_addresses.json").unwrap();
        SwapEncoderRegistry::new(eth_chain())
            .add_default_encoders(Some(executors_addresses))
            .unwrap()
    }

    fn get_tycho_router_encoder() -> TychoRouterEncoder {
        TychoRouterEncoder::new(get_swap_encoder_registry(), router_address()).unwrap()
    }

    mod router_encoder {
        use std::time::{Duration, Instant};

        use alloy::hex::encode;

        use super::*;
        use crate::encoding::evm::testing_utils::delayed_bebop_swap;

        /// Two solutions each hold one RFQ swap that waits 300ms for its quote. Encoded together
        /// they finish well before the 600ms a one-after-the-other encoding needs, and stay in
        /// input order.
        #[test]
        fn test_encode_solutions_encodes_rfq_solutions_in_parallel() {
            let encoder = get_tycho_router_encoder();
            let delay = Duration::from_millis(300);
            let single_bebop_solution = |token_in: Bytes, token_out: Bytes| {
                Solution::new(
                    Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                    Bytes::default(),
                    token_in.clone(),
                    token_out.clone(),
                    BigUint::from(1_000u64),
                    BigUint::from(1_000u64),
                    BigUint::from(900u64),
                    vec![delayed_bebop_swap(token_in, token_out, delay)],
                )
            };
            let solutions =
                vec![single_bebop_solution(usdc(), weth()), single_bebop_solution(dai(), wbtc())];

            let start = Instant::now();
            let encoded_solutions = encoder
                .encode_solutions(solutions)
                .unwrap();
            let elapsed = start.elapsed();

            assert!(elapsed < Duration::from_millis(500), "encoding took {elapsed:?}");
            assert_eq!(encoded_solutions.len(), 2);
            assert!(encode(encoded_solutions[0].swaps()).contains(&encode(usdc())[..]));
            assert!(encode(encoded_solutions[1].swaps()).contains(&encode(dai())[..]));
        }

        /// A batch that holds one invalid solution fails before the valid solutions request their
        /// signed quotes, so the 300ms RFQ round trip never runs.
        #[test]
        fn test_encode_solutions_rejects_an_invalid_solution_before_requesting_quotes() {
            let encoder = get_tycho_router_encoder();
            let delay = Duration::from_millis(300);
            let delayed_solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc(),
                weth(),
                BigUint::from(1_000u64),
                BigUint::from(1_000u64),
                BigUint::from(900u64),
                vec![delayed_bebop_swap(usdc(), weth(), delay)],
            );
            let solution_without_swaps = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                dai(),
                wbtc(),
                BigUint::from(1_000u64),
                BigUint::from(1_000u64),
                BigUint::from(900u64),
                vec![],
            );

            let start = Instant::now();
            let result = encoder.encode_solutions(vec![delayed_solution, solution_without_swaps]);
            let elapsed = start.elapsed();

            assert!(matches!(result, Err(EncodingError::FatalError(_))), "{result:?}");
            assert!(elapsed < delay, "encoding took {elapsed:?}");
        }

        #[test]
        fn test_encode_router_calldata_split_swap_group() {
            let encoder = get_tycho_router_encoder();
            let swap_usdc_eth = swap_usdc_eth_univ4().with_split(0.5);
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc(),
                eth(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                vec![swap_usdc_eth, swap_usdc_eth_univ4()],
            );

            let encoded_solution_res = encoder.encode_solution(&solution);
            assert!(encoded_solution_res.is_ok());

            let encoded_solution = encoded_solution_res.unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("splitSwap"));
        }

        /// Builds a uniswap_v2 swap between two ERC-20 tokens with a split
        /// (0.0 means "take the remainder").
        fn univ2_swap(token_in: &Bytes, token_out: &Bytes, split: f64) -> Swap {
            Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            )
            .with_split(split)
        }

        /// Builds a swap on the native_wrapper component the Tycho stream injects.
        fn wrap_swap(token_in: &Bytes, token_out: &Bytes, split: f64) -> Swap {
            Swap::new(
                ProtocolComponent {
                    protocol_system: "native_wrapper".to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            )
            .with_split(split)
        }

        /// A sequential solution carrying its wrap swap explicitly:
        /// USDC -> ETH -> WETH, where ETH -> WETH runs on the native_wrapper component.
        #[test]
        fn test_encode_explicit_wrap_swap() {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc(),
                weth(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                vec![swap_usdc_eth_univ4(), wrap_swap(&eth(), &weth(), 0.0)],
            );

            let encoded_solution = encoder
                .encode_solution(&solution)
                .unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("sequentialSwap"));
        }

        /// A split solution whose wrap swap converts only part of the balance:
        /// 60% of the ETH input is wrapped for the WETH branch, the remainder
        /// swaps as native ETH.
        //
        //       ┌──[wrap 60%]── WETH ──┐
        // ETH ──┤                      ├── USDC
        //       └──[rem]───────────────┘
        #[test]
        fn test_encode_explicit_wrap_swap_with_split() {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                eth(),
                usdc(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                vec![
                    wrap_swap(&eth(), &weth(), 0.6),
                    univ2_swap(&weth(), &usdc(), 0.0),
                    univ2_swap(&eth(), &usdc(), 0.0),
                ],
            );

            let encoded_solution = encoder
                .encode_solution(&solution)
                .unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("splitSwap"));
        }

        /// A complete split solution with parallel WETH and ETH branches. The encoder
        /// must not insert a wrap swap between them; the solution must encode as given.
        //
        //       ┌──[70%]── WETH ──┐
        // DAI ──┤                 ├── USDC
        //       └──[rem]── ETH ───┘
        #[test]
        fn test_encode_split_solution_with_native_and_wrapped_branches() {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                dai(),
                usdc(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                vec![
                    univ2_swap(&dai(), &weth(), 0.7),
                    univ2_swap(&dai(), &eth(), 0.0),
                    univ2_swap(&weth(), &usdc(), 0.0),
                    univ2_swap(&eth(), &usdc(), 0.0),
                ],
            );

            let encoded_solution = encoder
                .encode_solution(&solution)
                .unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("splitSwap"));
        }

        /// A solution with an ETH↔WETH gap and no wrap swap: the encoder no longer fills
        /// it, so the strategy's swap-path validation rejects the unconnected token.
        #[rstest]
        // token_in is ETH, the swap consumes WETH
        #[case::gap_at_start(eth(), dai(), vec![univ2_swap(&weth(), &dai(), 0.0)])]
        // token_out is WETH, the swap produces ETH
        #[case::gap_at_end(usdc(), weth(), vec![swap_usdc_eth_univ4()])]
        // the second swap consumes WETH, but the route only holds ETH
        #[case::gap_mid_route(
            usdc(),
            dai(),
            vec![swap_usdc_eth_univ4(), univ2_swap(&weth(), &dai(), 0.0)]
        )]
        fn test_validate_native_wrap_gap(
            #[case] token_in: Bytes,
            #[case] token_out: Bytes,
            #[case] swaps: Vec<Swap>,
        ) {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                token_in,
                token_out,
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000000000000000").unwrap(),
                BigUint::from_str("970_000000000000000000").unwrap(),
                swaps,
            );

            let result = encoder.encode_solution(&solution);

            assert!(matches!(result, Err(EncodingError::InvalidInput(_))), "{result:?}");
        }

        #[test]
        fn test_validate_fails_no_swaps() {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::default(),
                Bytes::default(),
                eth(),
                Bytes::default(),
                BigUint::default(),
                BigUint::default(),
                BigUint::default(),
                vec![],
            );

            let result = encoder.validate_solution(&solution);

            assert!(result.is_err());
            assert_eq!(
                result.err().unwrap(),
                EncodingError::FatalError("No swaps found in solution".to_string())
            );
        }

        #[test]
        fn test_validate_fails_zero_expected_amount_out() {
            let encoder = get_tycho_router_encoder();
            let swap = Swap::new(
                ProtocolComponent {
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth()),
                default_token(dai()),
                BigUint::ZERO,
            );
            let solution = Solution::new(
                Bytes::default(),
                Bytes::default(),
                weth(),
                dai(),
                BigUint::from(1u64),
                BigUint::ZERO,
                BigUint::ZERO,
                vec![swap],
            );

            let result = encoder.validate_solution(&solution);

            assert!(result.is_err());
            assert_eq!(
                result.err().unwrap(),
                EncodingError::FatalError(
                    "Solution expected_amount_out must be non-zero: the router rejects a zero \
                     expectedAmountOut"
                        .to_string()
                )
            );
        }

        #[test]
        fn test_validate_cyclical_swap() {
            // This validation passes because the cyclical swap is the first and last token
            //      50% ->  WETH
            // DAI -              -> DAI
            //      50% -> WETH
            // (some of the pool addresses in this test are fake)
            let encoder = get_tycho_router_encoder();
            let swaps = vec![
                Swap::new(
                    ProtocolComponent {
                        id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai().clone()),
                    default_token(weth().clone()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai().clone()),
                    default_token(weth().clone()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(weth().clone()),
                    default_token(dai().clone()),
                    BigUint::ZERO,
                ),
            ];

            let solution = Solution::new(
                Bytes::default(),
                Bytes::default(),
                dai(),
                dai(),
                BigUint::default(),
                BigUint::from(1u64),
                BigUint::from(1u64),
                swaps,
            );

            let result = encoder.validate_solution(&solution);

            assert!(result.is_ok());
        }

        #[test]
        fn test_validate_cyclical_swap_fail() {
            // This test should fail because the cyclical swap is not the first and last token
            // DAI -> WETH -> USDC -> DAI -> WBTC
            // (some of the pool addresses in this test are fake)
            let encoder = get_tycho_router_encoder();
            let swaps = vec![
                Swap::new(
                    ProtocolComponent {
                        id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai().clone()),
                    default_token(weth().clone()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(weth().clone()),
                    default_token(usdc().clone()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(usdc().clone()),
                    default_token(dai().clone()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai().clone()),
                    default_token(wbtc().clone()),
                    BigUint::ZERO,
                ),
            ];

            let solution = Solution::new(
                Bytes::default(),
                Bytes::default(),
                dai(),
                wbtc(),
                BigUint::default(),
                BigUint::from(1u64),
                BigUint::from(1u64),
                swaps,
            );

            let result = encoder.validate_solution(&solution);

            assert!(result.is_err());
            assert_eq!(
            result.err().unwrap(),
            EncodingError::FatalError(
                "Cyclical swaps are only allowed if they are the first and last token of a solution".to_string()
            )
        );
        }
        #[test]
        fn test_validate_cyclical_swap_split_output() {
            // This validation passes because it is a valid cyclical swap
            //             -> WETH
            // WETH -> DAI
            //             -> WETH
            // (some of the pool addresses in this test are fake)
            let encoder = get_tycho_router_encoder();
            let swaps = vec![
                Swap::new(
                    ProtocolComponent {
                        id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(weth()),
                    default_token(dai()),
                    BigUint::ZERO,
                ),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai()),
                    default_token(weth()),
                    BigUint::ZERO,
                )
                .with_split(0.5),
                Swap::new(
                    ProtocolComponent {
                        id: "0x0000000000000000000000000000000000000000".to_string(),
                        protocol_system: "uniswap_v2".to_string(),
                        ..Default::default()
                    },
                    default_token(dai()),
                    default_token(weth()),
                    BigUint::ZERO,
                ),
            ];

            let solution = Solution::new(
                Bytes::default(),
                Bytes::default(),
                weth(),
                weth(),
                BigUint::default(),
                BigUint::from(1u64),
                BigUint::from(1u64),
                swaps,
            );

            let result = encoder.validate_solution(&solution);

            assert!(result.is_ok());
        }
    }
}
