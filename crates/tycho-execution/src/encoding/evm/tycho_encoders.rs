use std::collections::HashSet;

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        group_swaps::group_swaps,
        strategy_encoder::strategy_encoders::{
            SequentialSwapStrategyEncoder, SingleSwapStrategyEncoder, SplitSwapStrategyEncoder,
        },
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::ple_encode,
    },
    models::{EncodedSolution, EncodingContext, Solution, Swap},
    strategy_encoder::StrategyEncoder,
    tycho_encoder::TychoEncoder,
};

/// Encodes solutions to be used by the TychoRouterV3.
///
/// # Fields
/// * `chain`: Chain to be used
/// * `single_swap_strategy`: Encoder for single swaps
/// * `sequential_swap_strategy`: Encoder for sequential swaps
/// * `split_swap_strategy`: Encoder for split swaps
/// * `router_address`: Address of the Tycho router contract
#[derive(Clone)]
pub(crate) struct TychoRouterEncoder {
    chain: Chain,
    single_swap_strategy: SingleSwapStrategyEncoder,
    sequential_swap_strategy: SequentialSwapStrategyEncoder,
    split_swap_strategy: SplitSwapStrategyEncoder,
}

impl TychoRouterEncoder {
    pub(crate) fn new(
        chain: Chain,
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
                swap_encoder_registry,
                router_address.clone(),
            )?,
            chain,
        })
    }

    fn encode_solution(&self, solution: &Solution) -> Result<EncodedSolution, EncodingError> {
        self.validate_solution(solution)?;
        let solution = self.add_native_wrap_swaps(solution, &self.chain);

        let groups = group_swaps(solution.swaps());

        let encoded_solution = if groups.len() == 1 {
            self.single_swap_strategy
                .encode_strategy(&solution)?
        } else if solution
            .swaps()
            .iter()
            .all(|swap| swap.split() == 0.0)
        {
            self.sequential_swap_strategy
                .encode_strategy(&solution)?
        } else {
            self.split_swap_strategy
                .encode_strategy(&solution)?
        };

        Ok(encoded_solution)
    }

    /// Returns a new solution with added wrapping/unwrapping swaps if the
    /// original solution contains a swap between a chain's native token and
    /// its wrapped counterpart but doesn't include the corresponding
    /// wrapping or unwrapping swap.
    ///
    /// This assumes the swaps are already in a valid execution order: a swap comes after the
    /// swaps that produce its input token, and for each token the 0%-split swap that takes the
    /// remainder comes last. Reordering the swaps invalidates the inserted wrap swaps.
    ///
    /// A wrap swap is inserted directly before the first swap that consumes a token which
    /// neither the solution's input nor an earlier swap provides. That position is after every
    /// swap that takes a fraction of the wrap swap's input token, which the split ordering rules
    /// require. `missing_wrap_swap` lists the conditions for an insertion, and decides whether
    /// the wrap swap takes the whole remaining balance or only a share of it.
    ///
    /// `available_tokens` tracks the tokens that still hold a balance at each point of the
    /// route. A 0%-split swap takes its input token's whole remainder, so it also removes
    /// that token again.
    ///
    /// `validate_native_wrap_gaps` has already rejected the solutions where a needed conversion
    /// amount is ambiguous, so an unservable gap here simply inserts nothing.
    fn add_native_wrap_swaps(&self, solution: &Solution, chain: &Chain) -> Solution {
        let swaps = solution.swaps();
        let mut new_swaps: Vec<Swap> = Vec::with_capacity(swaps.len());
        let mut available_tokens = HashSet::from([solution.token_in().clone()]);

        for (i, swap) in swaps.iter().enumerate() {
            if let Some(wrap_swap) = self.missing_wrap_swap(
                &swap.token_in().address,
                &available_tokens,
                &swaps[i..],
                solution,
                chain,
            ) {
                if wrap_swap.split() == 0.0 {
                    available_tokens.remove(&wrap_swap.token_in().address);
                }
                available_tokens.insert(wrap_swap.token_out().address.clone());
                new_swaps.push(wrap_swap);
            }
            if swap.split() == 0.0 {
                available_tokens.remove(&swap.token_in().address);
            }
            available_tokens.insert(swap.token_out().address.clone());
            new_swaps.push(swap.clone());
        }

        // Check if we need to add an unwrapping swap at the end of the solution
        if let Some(last_swap) = swaps.last() {
            if let Some(unwrap_swap) =
                self.wrap_swap_between(&last_swap.token_out().address, solution.token_out(), chain)
            {
                new_swaps.push(unwrap_swap);
            }
        }

        solution.clone().with_swaps(new_swaps)
    }

    /// Returns the wrap swap that must run before a swap that consumes `consumed_token`,
    /// or `None` if no wrap swap is needed or possible.
    ///
    /// `remaining_swaps` are the solution's swaps from the consuming swap onwards.
    ///
    /// A wrap swap is needed when `consumed_token` is not available yet and the balance of its
    /// native/wrapped counterpart can supply it, meaning both of:
    /// * the counterpart still holds a balance;
    /// * the counterpart is not the solution's output token — that balance is the payout, and
    ///   wrapping it would re-route it through later swaps and deliver less than quoted.
    ///
    /// The wrap swap takes the counterpart's whole remaining balance unless a later swap still
    /// consumes the counterpart, in which case it takes only the share `wrap_share` derives.
    /// Without a derivable share nothing is inserted; `validate_native_wrap_gaps` rejects those
    /// solutions before they reach here.
    fn missing_wrap_swap(
        &self,
        consumed_token: &Bytes,
        available_tokens: &HashSet<Bytes>,
        remaining_swaps: &[Swap],
        solution: &Solution,
        chain: &Chain,
    ) -> Option<Swap> {
        if available_tokens.contains(consumed_token) {
            return None;
        }
        let native = chain.native_token().address;
        let wrapped = chain.wrapped_native_token().address;
        let counterpart = if *consumed_token == native {
            wrapped
        } else if *consumed_token == wrapped {
            native
        } else {
            return None;
        };

        if !available_tokens.contains(&counterpart) || counterpart == *solution.token_out() {
            return None;
        }
        let wrap_swap = self.wrap_swap_between(&counterpart, consumed_token, chain)?;

        let counterpart_consumed_later = remaining_swaps
            .iter()
            .any(|swap| swap.token_in().address == counterpart);
        if !counterpart_consumed_later {
            return Some(wrap_swap);
        }
        let share = wrap_share(consumed_token, &counterpart, solution.swaps(), remaining_swaps)?;
        Some(wrap_swap.with_split(share))
    }

    /// Raises an `EncodingError` if a swap consumes a native or wrapped token that no swap
    /// provides and the amount to convert from its counterpart does not follow from the solution.
    ///
    /// The amount is ambiguous when the counterpart's balance is the solution's payout, or when a
    /// later swap also consumes the counterpart and `wrap_share` cannot derive a share. Either way
    /// the solver has to emit an explicit `native_wrapper` swap carrying the split it wants.
    ///
    /// A token that neither the solution nor its counterpart can supply is left to the strategy
    /// validators, which report it as an unconnected token.
    fn validate_native_wrap_gaps(&self, solution: &Solution) -> Result<(), EncodingError> {
        let swaps = solution.swaps();
        let native = self.chain.native_token().address;
        let wrapped = self
            .chain
            .wrapped_native_token()
            .address;

        for (i, swap) in swaps.iter().enumerate() {
            let consumed_token = &swap.token_in().address;
            let counterpart = if *consumed_token == native {
                &wrapped
            } else if *consumed_token == wrapped {
                &native
            } else {
                continue;
            };
            if is_supplied(consumed_token, solution) || !is_supplied(counterpart, solution) {
                continue;
            }
            if *counterpart == *solution.token_out() {
                return Err(EncodingError::InvalidInput(format!(
                    "A swap consumes {consumed_token} which no swap provides, and its counterpart \
                     {counterpart} is the solution's output token. Converting that balance would \
                     spend the payout. Add an explicit native_wrapper swap with the split to \
                     convert."
                )));
            }
            let remaining_swaps = &swaps[i..];
            let counterpart_consumed_later = remaining_swaps
                .iter()
                .any(|swap| swap.token_in().address == *counterpart);
            if counterpart_consumed_later &&
                wrap_share(consumed_token, counterpart, swaps, remaining_swaps).is_none()
            {
                return Err(EncodingError::InvalidInput(format!(
                    "A swap consumes {consumed_token} which no swap provides, and a later swap \
                     also consumes its counterpart {counterpart}. The amount to convert does not \
                     follow from the solution. Add an explicit native_wrapper swap with the split \
                     to convert, or set estimated_amount_in on the swaps that consume both tokens."
                )));
            }
        }
        Ok(())
    }

    /// Returns the swap that wraps or unwraps `token_in` into `token_out`, or `None`
    /// if the two tokens are not the chain's native/wrapped-native pair.
    fn wrap_swap_between(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
        chain: &Chain,
    ) -> Option<Swap> {
        let native = chain.native_token();
        let wrapped_native = chain.wrapped_native_token();
        let wrap_component = ProtocolComponent {
            protocol_system: "native_wrapper".to_string(),
            ..Default::default()
        };

        if token_in == &wrapped_native.address && token_out == &native.address {
            Some(Swap::new(wrap_component, wrapped_native, native, BigUint::from(14_000u64)))
        } else if token_in == &native.address && token_out == &wrapped_native.address {
            Some(Swap::new(wrap_component, native, wrapped_native, BigUint::from(7_000u64)))
        } else {
            None
        }
    }
}

/// Returns whether `token` holds a balance at some point of the route, either because the
/// solution starts from it or because a swap produces it.
fn is_supplied(token: &Bytes, solution: &Solution) -> bool {
    token == solution.token_in() ||
        solution
            .swaps()
            .iter()
            .any(|swap| swap.token_out().address == *token)
}

/// Returns the fraction of `counterpart`'s balance that must be wrapped to supply
/// `consumed_token`, or `None` if the solution does not determine it.
///
/// Wrapping is 1:1, so the share follows from the solver's `estimated_amount_in` values:
/// the amount the `consumed_token` swaps need, over that amount plus the amount the swap that
/// takes the counterpart's remainder needs.
///
/// This only covers the shape where the share is unambiguous:
/// * no swap produces `consumed_token`, so every swap consuming it must be supplied by wrapping;
/// * exactly one swap from this position onwards consumes `counterpart`, and it takes the
///   remainder. With several the existing splits could be relative to the balance either before or
///   after wrapping, and picking the wrong reading would misroute funds;
/// * every swap involved carries an `estimated_amount_in`.
fn wrap_share(
    consumed_token: &Bytes,
    counterpart: &Bytes,
    swaps: &[Swap],
    remaining_swaps: &[Swap],
) -> Option<f64> {
    if swaps
        .iter()
        .any(|swap| swap.token_out().address == *consumed_token)
    {
        return None;
    }

    let mut wrapped_amount = BigUint::ZERO;
    for swap in swaps
        .iter()
        .filter(|swap| swap.token_in().address == *consumed_token)
    {
        wrapped_amount += swap.estimated_amount_in().clone()?;
    }

    let mut counterpart_consumers = remaining_swaps
        .iter()
        .filter(|swap| swap.token_in().address == *counterpart);
    let remainder_swap = counterpart_consumers.next()?;
    if counterpart_consumers.next().is_some() || remainder_swap.split() != 0.0 {
        return None;
    }
    let counterpart_amount = remainder_swap
        .estimated_amount_in()
        .clone()?;

    let total = &wrapped_amount + &counterpart_amount;
    let share = wrapped_amount.to_f64()? / total.to_f64()?;
    if share > 0.0 && share < 1.0 {
        Some(share)
    } else {
        None
    }
}

impl TychoEncoder for TychoRouterEncoder {
    fn encode_solutions(
        &self,
        solutions: Vec<Solution>,
    ) -> Result<Vec<EncodedSolution>, EncodingError> {
        let mut result: Vec<EncodedSolution> = Vec::new();
        for solution in solutions.iter() {
            let encoded_solution = self.encode_solution(solution)?;
            result.push(encoded_solution);
        }
        Ok(result)
    }

    /// Raises an `EncodingError` if the solution is not considered valid.
    ///
    /// A solution is considered valid if all the following conditions are met:
    /// * The solution has at least one swap.
    /// * The quoted `expected_amount_out` is non-zero (the router rejects a zero
    ///   `expectedAmountOut`).
    /// * The token cannot appear more than once in the solution unless it is the first and last
    ///   token (i.e. a true cyclical swap).
    /// * Where a swap consumes a native or wrapped token that no swap provides, the amount to
    ///   convert from its counterpart follows from the solution. `validate_native_wrap_gaps` lists
    ///   what makes that amount ambiguous.
    fn validate_solution(&self, solution: &Solution) -> Result<(), EncodingError> {
        self.validate_native_wrap_gaps(solution)?;
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

/// Represents an encoder for one swap to be executed directly against an Executor.
///
/// This is useful when you want to bypass the Tycho Router, use your own Router contract and
/// just need the calldata for a particular swap.
///
/// # Fields
/// * `swap_encoder_registry`: Registry of swap encoders
#[derive(Clone)]
pub(crate) struct TychoExecutorEncoder {
    swap_encoder_registry: SwapEncoderRegistry,
}

impl TychoExecutorEncoder {
    pub(crate) fn new(swap_encoder_registry: SwapEncoderRegistry) -> Result<Self, EncodingError> {
        Ok(TychoExecutorEncoder { swap_encoder_registry })
    }

    fn encode_executor_calldata(
        &self,
        solution: &Solution,
    ) -> Result<EncodedSolution, EncodingError> {
        let grouped_swaps = group_swaps(solution.swaps());
        let number_of_groups = grouped_swaps.len();
        if number_of_groups > 1 {
            return Err(EncodingError::InvalidInput(format!(
                "Tycho executor encoder only supports one swap. Found {number_of_groups}"
            )));
        }

        let grouped_swap = grouped_swaps
            .first()
            .ok_or_else(|| EncodingError::FatalError("Swap grouping failed".to_string()))?;

        let swap_encoder = self
            .swap_encoder_registry
            .get_encoder(&grouped_swap.protocol_system)
            .ok_or_else(|| {
                EncodingError::InvalidInput(format!(
                    "Swap encoder not found for protocol: {}",
                    grouped_swap.protocol_system
                ))
            })?;

        let encoding_context = EncodingContext {
            router_address: None,
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
            initial_protocol_data.extend(ple_encode(grouped_protocol_data));
        }

        Ok(EncodedSolution::new(
            initial_protocol_data,
            swap_encoder.executor_address().clone(),
            "".to_string(),
            0,
            grouped_swap.estimated_gas.clone(),
        ))
    }
}

impl TychoEncoder for TychoExecutorEncoder {
    fn encode_solutions(
        &self,
        solutions: Vec<Solution>,
    ) -> Result<Vec<EncodedSolution>, EncodingError> {
        let solution = solutions
            .first()
            .ok_or(EncodingError::FatalError("No solutions found".to_string()))?;
        self.validate_solution(solution)?;

        let encoded_solution = self.encode_executor_calldata(solution)?;

        Ok(vec![encoded_solution])
    }

    /// Raises an `EncodingError` if the solution is not considered valid.
    fn validate_solution(&self, _solution: &Solution) -> Result<(), EncodingError> {
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

    fn pepe() -> Bytes {
        Bytes::from_str("0x6982508145454Ce325dDbE47a25d4ec3d2311933").unwrap()
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

    fn swap_eth_pepe_univ4() -> Swap {
        let pool_fee_eth_pepe = Bytes::from(BigInt::from(25000).to_signed_bytes_be());
        let tick_spacing_eth_pepe = Bytes::from(BigInt::from(500).to_signed_bytes_be());
        let mut static_attributes_eth_pepe: HashMap<String, Bytes> = HashMap::new();
        static_attributes_eth_pepe.insert("key_lp_fee".into(), pool_fee_eth_pepe);
        static_attributes_eth_pepe.insert("tick_spacing".into(), tick_spacing_eth_pepe);
        Swap::new(
            ProtocolComponent {
                id: "0xecd73ecbf77219f21f129c8836d5d686bbc27d264742ddad620500e3e548e2c9"
                    .to_string(),
                protocol_system: "uniswap_v4".to_string(),
                static_attributes: static_attributes_eth_pepe,
                ..Default::default()
            },
            default_token(eth().clone()),
            default_token(pepe().clone()),
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
        TychoRouterEncoder::new(eth_chain(), get_swap_encoder_registry(), router_address()).unwrap()
    }

    mod router_encoder {
        use super::*;
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

        #[test]
        fn test_add_missing_wrapped_eth_swap_in_the_middle() {
            // before adding swap: DAI -> USDC -> ETH (no swap) WETH -> DAI
            // after adding swap:  DAI -> USDC -> ETH -> WETH -> DAI

            let encoder = get_tycho_router_encoder();

            let swap_dai_usdc = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(dai().clone()),
                default_token(usdc().clone()),
                BigUint::ZERO,
            );

            let swap_weth_dai = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth().clone()),
                default_token(dai().clone()),
                BigUint::ZERO,
            );

            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                dai(),
                dai(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                vec![swap_dai_usdc, swap_usdc_eth_univ4(), swap_weth_dai],
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps().len(), 4);
            assert_eq!(solution.swaps()[2].token_in().address, eth());
            assert_eq!(solution.swaps()[2].token_out().address, weth());
            assert_eq!(
                solution.swaps()[2]
                    .component()
                    .protocol_system,
                "native_wrapper"
            );
        }

        #[test]
        fn test_add_missing_wrapped_eth_swap_in_the_beginning() {
            // before adding swap: ETH is the solution token_in, WETH -> DAI
            // after adding swap:  ETH -> WETH -> DAI

            let encoder = get_tycho_router_encoder();

            let swap_weth_dai = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth().clone()),
                default_token(dai().clone()),
                BigUint::ZERO,
            );

            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                eth(),
                dai(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                vec![swap_weth_dai],
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps().len(), 2);
            assert_eq!(solution.swaps()[0].token_in().address, eth());
            assert_eq!(solution.swaps()[0].token_out().address, weth());
            assert_eq!(
                solution.swaps()[0]
                    .component()
                    .protocol_system,
                "native_wrapper"
            );
        }

        #[test]
        fn test_add_missing_wrapped_eth_swap_in_the_end() {
            // before adding swap: USDC -> ETH, WETH is the solution token_out
            // after adding swap:  USDC -> ETH -> WETH

            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc(),
                weth(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                vec![swap_usdc_eth_univ4()],
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            let last_swap = solution.swaps().last().unwrap();
            assert_eq!(solution.swaps().len(), 2);
            assert_eq!(last_swap.token_in().address, eth());
            assert_eq!(last_swap.token_out().address, weth());
            assert_eq!(last_swap.component().protocol_system, "native_wrapper");
        }

        /// Builds a uniswap_v2 swap between two ERC-20 tokens with an optional split.
        fn univ2_swap(token_in: &Bytes, token_out: &Bytes, split: f64) -> Swap {
            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            );
            swap.with_split(split)
        }

        /// A split solution with parallel ETH and WETH branches and no dangling balance:
        /// no wrap swap may be inserted. In `split_branch_boundary` the DAI→ETH swap's
        /// output is fully consumed by the ETH→USDC branch. In `native_payout` the ETH
        /// produced by the DAI→ETH swap is the solution's output, so nothing consumes it.
        #[rstest]
        //       ┌──[70%]── WETH ──┐
        // DAI ──┤                 ├── USDC   (ETH is consumed by its own branch)
        //       └──[rem]── ETH ───┘
        #[case::split_branch_boundary(
            vec![
                univ2_swap(&dai(), &weth(), 0.7),
                univ2_swap(&dai(), &eth(), 0.0),
                univ2_swap(&weth(), &usdc(), 0.0),
                univ2_swap(&eth(), &usdc(), 0.0),
            ],
            usdc()
        )]
        //       ┌──[60%]── WETH ── USDC ──┐
        // DAI ──┤                         ├── ETH   (both branches deliver the payout)
        //       └──[rem]──────────────────┘
        #[case::native_payout(
            vec![
                univ2_swap(&dai(), &weth(), 0.6),
                univ2_swap(&dai(), &eth(), 0.0),
                univ2_swap(&weth(), &usdc(), 0.0),
                univ2_swap(&usdc(), &eth(), 0.0),
            ],
            eth()
        )]
        fn test_add_native_wrap_swaps_non_dangling_balance(
            #[case] input_swaps: Vec<Swap>,
            #[case] token_out: Bytes,
        ) {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                dai(),
                token_out,
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                input_swaps.clone(),
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps(), input_swaps.as_slice());
        }

        /// A split solution where one branch takes a fraction of the ETH (or WETH)
        /// balance and the other branch consumes the counterpart token: the remainder
        /// dangles, so a wrap (or unwrap) swap must be inserted after the fractional
        /// consumer, and the solution must still encode as a split swap.
        #[rstest]
        //                ┌──[30%]───────────┐
        // DAI ─── ETH ───┤                  ├── USDC
        //                └──[rem]── WETH ───┘
        #[case::wrap_remainder(eth(), weth())]
        //                ┌──[30%]───────────┐
        // DAI ─── WETH ──┤                  ├── USDC
        //                └──[rem]── ETH ────┘
        #[case::unwrap_remainder(weth(), eth())]
        fn test_add_native_wrap_swaps_dangling_remainder(
            #[case] remainder_token: Bytes,
            #[case] counterpart_token: Bytes,
        ) {
            let encoder = get_tycho_router_encoder();
            let input_swaps = vec![
                univ2_swap(&dai(), &remainder_token, 0.0),
                univ2_swap(&remainder_token, &usdc(), 0.3),
                univ2_swap(&counterpart_token, &usdc(), 0.0),
            ];
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                dai(),
                usdc(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                input_swaps.clone(),
            );

            let wrapped_solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);

            let swaps = wrapped_solution.swaps();
            assert_eq!(swaps.len(), 4);
            assert_eq!(swaps[..2], input_swaps[..2]);
            assert_eq!(swaps[2].component().protocol_system, "native_wrapper");
            assert_eq!(swaps[2].token_in().address, remainder_token);
            assert_eq!(swaps[2].token_out().address, counterpart_token);
            assert_eq!(swaps[2].split(), 0.0);
            assert_eq!(swaps[3], input_swaps[2]);

            let encoded_solution = encoder
                .encode_solution(&solution)
                .unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("splitSwap"));
        }

        /// End-to-end regression: the split solution below must encode as a split swap
        /// instead of failing validation on an injected 0%-split wrap.
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

        /// The same shape as `test_add_native_wrap_swaps_partial_share`, but no swap carries an
        /// estimated amount, so the share to convert does not follow from the solution.
        #[rstest]
        #[case::wrapped_branch_first(eth(), weth())]
        #[case::native_branch_first(weth(), eth())]
        fn test_validate_fails_undetermined_wrap_share(
            #[case] input_token: Bytes,
            #[case] counterpart_token: Bytes,
        ) {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                input_token.clone(),
                usdc(),
                BigUint::from(1000u64),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                vec![
                    univ2_swap(&counterpart_token, &usdc(), 0.0),
                    univ2_swap(&input_token, &usdc(), 0.0),
                ],
            );

            let result = encoder.validate_solution(&solution);

            let Err(EncodingError::InvalidInput(message)) = result else {
                panic!("expected an InvalidInput error, got {result:?}");
            };
            assert!(message.contains("does not follow from the solution"), "{message}");
        }

        /// A cyclical solution whose mid-route branch needs the counterpart of the output token.
        /// That balance is the payout, so converting it would deliver less than quoted.
        //
        //        ┌──[50%]── USDC ──┐
        // WETH ──┤                 ├── WETH
        //        └── (ETH) ── DAI ─┘
        #[test]
        fn test_validate_fails_wrap_would_spend_payout() {
            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth(),
                weth(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("1010_000000000000000000").unwrap(),
                BigUint::from_str("1005_000000000000000000").unwrap(),
                vec![
                    univ2_swap(&weth(), &usdc(), 0.5),
                    univ2_swap(&eth(), &dai(), 0.0),
                    univ2_swap(&usdc(), &weth(), 0.0),
                    univ2_swap(&dai(), &weth(), 0.0),
                ],
            );

            let result = encoder.validate_solution(&solution);

            let Err(EncodingError::InvalidInput(message)) = result else {
                panic!("expected an InvalidInput error, got {result:?}");
            };
            assert!(message.contains("output token"), "{message}");
        }

        /// The solution's input token feeds one branch directly and its counterpart feeds
        /// another. Wrapping the whole input balance to serve the counterpart branch would
        /// starve the branch that consumes the input token, so no wrap swap may be inserted.
        #[rstest]
        //       ┌── WETH ──┐
        // ETH ──┤          ├── USDC
        //       └── ETH ───┘
        #[case::wrapped_branch_first(eth(), weth())]
        //        ┌── ETH ───┐
        // WETH ──┤          ├── USDC
        //        └── WETH ──┘
        #[case::native_branch_first(weth(), eth())]
        fn test_add_native_wrap_swaps_input_consumed_by_later_branch(
            #[case] input_token: Bytes,
            #[case] counterpart_token: Bytes,
        ) {
            let encoder = get_tycho_router_encoder();
            let input_swaps = vec![
                univ2_swap(&counterpart_token, &usdc(), 0.0),
                univ2_swap(&input_token, &usdc(), 0.0),
            ];
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                input_token,
                usdc(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                input_swaps.clone(),
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps(), input_swaps.as_slice());
        }

        /// Builds a uniswap_v2 swap carrying the amount the solver routed through it.
        fn univ2_swap_with_amount(token_in: &Bytes, token_out: &Bytes, amount_in: u64) -> Swap {
            univ2_swap(token_in, token_out, 0.0).with_estimated_amount_in(BigUint::from(amount_in))
        }

        /// The solution's input token feeds one branch and its counterpart the other. The
        /// solver's estimated amounts say how much to convert, so the encoder inserts a wrap
        /// swap carrying that share instead of taking the whole balance.
        #[rstest]
        //       ┌──[wrap 60%]── WETH ──┐
        // ETH ──┤                      ├── USDC
        //       └──[rem]───────────────┘
        #[case::wrap_share(eth(), weth())]
        //        ┌──[unwrap 60%]── ETH ──┐
        // WETH ──┤                       ├── USDC
        //        └──[rem]────────────────┘
        #[case::unwrap_share(weth(), eth())]
        fn test_add_native_wrap_swaps_partial_share(
            #[case] input_token: Bytes,
            #[case] counterpart_token: Bytes,
        ) {
            let encoder = get_tycho_router_encoder();
            let input_swaps = vec![
                univ2_swap_with_amount(&counterpart_token, &usdc(), 600),
                univ2_swap_with_amount(&input_token, &usdc(), 400),
            ];
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                input_token.clone(),
                usdc(),
                BigUint::from(1000u64),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                input_swaps.clone(),
            );

            let wrapped_solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);

            let swaps = wrapped_solution.swaps();
            assert_eq!(swaps.len(), 3);
            assert_eq!(swaps[0].component().protocol_system, "native_wrapper");
            assert_eq!(swaps[0].token_in().address, input_token);
            assert_eq!(swaps[0].token_out().address, counterpart_token);
            assert!((swaps[0].split() - 0.6).abs() < 1e-12);
            assert_eq!(swaps[1..], input_swaps[..]);

            let encoded_solution = encoder
                .encode_solution(&solution)
                .unwrap();
            assert!(encoded_solution
                .function_signature()
                .contains("splitSwap"));
        }

        /// The counterpart's whole balance is already taken by an earlier 0%-split swap, so
        /// there is nothing left to wrap and no wrap swap may be inserted.
        //
        // ETH ── USDC ──┐
        //               ├── (WETH has no source) ── DAI ── USDC
        #[test]
        fn test_add_native_wrap_swaps_counterpart_already_drained() {
            let encoder = get_tycho_router_encoder();
            let input_swaps = vec![
                univ2_swap(&eth(), &usdc(), 0.0),
                univ2_swap(&weth(), &dai(), 0.0),
                univ2_swap(&dai(), &usdc(), 0.0),
            ];
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                eth(),
                usdc(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("990_000000").unwrap(),
                BigUint::from_str("970_000000").unwrap(),
                input_swaps.clone(),
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps(), input_swaps.as_slice());
        }

        /// A cyclical solution whose mid-route branch consumes the native counterpart of the
        /// output token. The encoder leaves the output token's balance alone: it cannot tell
        /// the payout apart from feedstock for that branch, and wrapping the payout would
        /// deliver less than quoted. A solver that wants the branch must emit the unwrap swap.
        //
        //        ┌──[50%]── USDC ──┐
        // WETH ──┤                 ├── WETH
        //        └── (ETH) ── DAI ─┘
        #[test]
        fn test_add_native_wrap_swaps_skips_cyclical_output_token() {
            let encoder = get_tycho_router_encoder();
            let input_swaps = vec![
                univ2_swap(&weth(), &usdc(), 0.5),
                univ2_swap(&eth(), &dai(), 0.0),
                univ2_swap(&usdc(), &weth(), 0.0),
                univ2_swap(&dai(), &weth(), 0.0),
            ];
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                weth(),
                weth(),
                BigUint::from_str("1000_000000000000000000").unwrap(),
                BigUint::from_str("1010_000000000000000000").unwrap(),
                BigUint::from_str("1005_000000000000000000").unwrap(),
                input_swaps.clone(),
            );

            let wrapped_solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(wrapped_solution.swaps(), input_swaps.as_slice());
        }

        #[test]
        fn test_sanity_check_no_missing_wrapped_eth_swap() {
            // USDC -> ETH -> WETH (no swap needed to be added)
            let eth_weth_swap = Swap::new(
                ProtocolComponent {
                    protocol_system: "native_wrapper".to_string(),
                    ..Default::default()
                },
                default_token(eth()),
                default_token(weth()),
                BigUint::ZERO,
            );

            let input_swaps = vec![swap_usdc_eth_univ4(), eth_weth_swap];

            let encoder = get_tycho_router_encoder();
            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc(),
                weth(),
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from_str("105_152_000000000000000000").unwrap(),
                BigUint::from_str("103048960000000000000000").unwrap(),
                input_swaps.clone(),
            );

            let solution = encoder.add_native_wrap_swaps(&solution, &encoder.chain);
            assert_eq!(solution.swaps().len(), 2);
            assert_eq!(solution.swaps(), input_swaps.as_slice());
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

    mod executor_encoder {
        use std::str::FromStr;

        use alloy::hex::encode;
        use num_bigint::BigUint;
        use tycho_common::{models::protocol::ProtocolComponent, Bytes};

        use super::*;
        use crate::encoding::models::Solution;

        #[test]
        fn test_executor_encoder_encode() {
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder = TychoExecutorEncoder::new(swap_encoder_registry).unwrap();

            let token_in = weth();
            let token_out = dai();

            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            );

            let solution = Solution::new(
                Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                Bytes::default(),
                token_in,
                token_out,
                BigUint::from(1000000000000000000u64),
                BigUint::from(1000000000000000000u64),
                BigUint::from(1000000000000000000u64),
                vec![swap],
            );

            let encoded_solutions = encoder
                .encode_solutions(vec![solution])
                .unwrap();
            let encoded = encoded_solutions
                .first()
                .expect("Expected at least one encoded solution");
            let hex_protocol_data = encode(encoded.swaps());
            assert_eq!(
                encoded.interacting_with(),
                &Bytes::from_str("0x5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").unwrap()
            );
            assert_eq!(
                hex_protocol_data,
                String::from(concat!(
                    // component id (pool address)
                    "a478c2975ab1ea89e8196811f51a7b7ade33eb11",
                    // tokenIn (WETH)
                    "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                    // tokenOut (DAI)
                    "6b175474e89094c44da98b954eedeac495271d0f",
                ))
            );
        }

        #[test]
        fn test_executor_encoder_too_many_swaps() {
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder = TychoExecutorEncoder::new(swap_encoder_registry).unwrap();

            let token_in = weth();
            let token_out = dai();

            let swap = Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            );

            let solution = Solution::new(
                Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                Bytes::default(),
                token_in,
                token_out,
                BigUint::from(1000000000000000000u64),
                BigUint::from(1000000000000000000u64),
                BigUint::from(1000000000000000000u64),
                vec![swap.clone(), swap],
            );

            let result = encoder.encode_solutions(vec![solution]);
            assert!(result.is_err());
        }

        #[test]
        fn test_executor_encoder_grouped_swaps() {
            let swap_encoder_registry = get_swap_encoder_registry();
            let encoder = TychoExecutorEncoder::new(swap_encoder_registry).unwrap();

            let usdc = usdc();
            let pepe = pepe();

            let solution = Solution::new(
                Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
                Bytes::default(),
                usdc,
                pepe,
                BigUint::from_str("1000_000000").unwrap(),
                BigUint::from(1000000000000000000u64),
                BigUint::from(1000000000000000000u64),
                vec![swap_usdc_eth_univ4(), swap_eth_pepe_univ4()],
            );

            let encoded_solutions = encoder
                .encode_solutions(vec![solution])
                .unwrap();
            let encoded_solution = encoded_solutions
                .first()
                .expect("Expected at least one encoded solution");
            let hex_protocol_data = encode(encoded_solution.swaps());
            assert_eq!(
                encoded_solution.interacting_with(),
                &Bytes::from_str("0xf62849f9a0b5bf2913b396098f7c7019b51a820a").unwrap()
            );
            assert_eq!(
                hex_protocol_data,
                String::from(concat!(
                    // group in token
                    "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    // group out token
                    "6982508145454ce325ddbe47a25d4ec3d2311933",
                    // zero for one
                    "00",
                    // skip unlock
                    "00",
                    // first pool intermediary token (ETH)
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                    // fee
                    "000bb8",
                    // tick spacing
                    "00003c",
                    // hook address (not set, so zero)
                    "0000000000000000000000000000000000000000",
                    // hook data length (0)
                    "0000",
                    // ple encoding
                    "0030",
                    // second pool intermediary token (PEPE)
                    "6982508145454ce325ddbe47a25d4ec3d2311933",
                    // fee
                    "0061a8",
                    // tick spacing
                    "0001f4",
                    // hook address (not set, so zero)
                    "0000000000000000000000000000000000000000",
                    // hook data length (0)
                    "0000",
                ))
            );
        }
    }
}
