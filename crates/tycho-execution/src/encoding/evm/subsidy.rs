use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{keccak256, B256, U256},
    sol_types::{eip712_domain, SolStruct},
};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::NATIVE_WRAPPER_PROTOCOL_SYSTEM,
        group_swaps::SwapGroup,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{biguint_to_u256, bytes_to_address, convert_to_router_token},
    },
    models::{Solution, SubsidySigningData, SubsidyTerms},
};

mod typed {
    alloy::sol! {
        struct Subsidy {
            address boundTo;
            address token;
            uint256 amount;
            uint256 minAmountIn;
            bytes32 innerSwapHash;
            uint256 nonce;
            uint256 deadline;
        }
    }
}

const PAYER_CONFIG_KEY: &str = "payer_address";
const MAX_ENCODABLE_DEADLINE: u64 = (1 << 48) - 1;
pub const SUBSIDY_SIGNATURE_LEN: usize = 65;

/// Start of the signature in the subsidizing executor's swap data.
pub const SUBSIDY_SIGNATURE_OFFSET: usize = 20 + 32 + 32 + 32 + 6;

/// The subsidizing executor and the subsidy payer of a chain.
#[derive(Clone, Debug)]
pub struct SubsidyEncoder {
    executor_address: Bytes,
    payer_address: Bytes,
    chain: Chain,
}

impl SubsidyEncoder {
    pub(crate) fn new(executor_address: Bytes, payer_address: Bytes, chain: Chain) -> Self {
        Self { executor_address, payer_address, chain }
    }

    /// Reads the payer address from `payer_address` in `config`.
    pub(crate) fn from_config(
        executor_address: Bytes,
        chain: Chain,
        config: Option<&HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let payer = config
            .and_then(|config| config.get(PAYER_CONFIG_KEY))
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "The subsidizing executor is configured for {chain}, but {PAYER_CONFIG_KEY} \
                     is missing from its subsidy protocol config"
                ))
            })?;
        let payer_address = Bytes::from_str(payer).map_err(|e| invalid_payer(payer, e))?;
        bytes_to_address(&payer_address).map_err(|e| invalid_payer(payer, e))?;
        Ok(Self::new(executor_address, payer_address, chain))
    }
}

/// The error for a configured payer address that is not a valid address.
fn invalid_payer(payer: &str, error: impl std::fmt::Display) -> EncodingError {
    EncodingError::FatalError(format!("Invalid subsidy {PAYER_CONFIG_KEY}: {payer}: {error}"))
}

/// The swap group that carries a solution's subsidy.
pub struct SubsidizedGroup<'a> {
    pub(crate) group_index: usize,
    token_out: &'a Bytes,
    terms: &'a SubsidyTerms,
    sender: &'a Bytes,
    encoder: &'a SubsidyEncoder,
}

/// Signing hash and signature position of the subsidized group, after wrapping.
pub struct SubsidizedGroupSignature {
    pub(crate) group_index: usize,
    signing_hash: B256,
    /// Distance from the start of the signature to the end of the group's protocol data.
    signature_from_end: usize,
}

impl SubsidizedGroupSignature {
    /// `swap_end` is where the group's bytes end in `swaps`.
    pub(crate) fn build_signing_data(&self, swap_end: usize) -> SubsidySigningData {
        SubsidySigningData {
            signing_hash: self.signing_hash,
            offset_in_swaps: swap_end - self.signature_from_end,
        }
    }
}

impl SubsidizedGroup<'_> {
    /// Replaces the group's executor and protocol data with the subsidizing executor's. The
    /// subsidy pays the group's output token.
    pub(crate) fn wrap(
        &self,
        executor_address: &mut Bytes,
        protocol_data: &mut Vec<u8>,
    ) -> Result<SubsidizedGroupSignature, EncodingError> {
        let bound_to = bytes_to_address(self.sender)?;
        let amount = biguint_to_u256(&self.terms.amount);
        let min_amount_in = biguint_to_u256(&self.terms.min_amount_in);

        let mut data = Vec::with_capacity(
            SUBSIDY_SIGNATURE_OFFSET + SUBSIDY_SIGNATURE_LEN + 20 + protocol_data.len(),
        );
        data.extend_from_slice(bound_to.as_slice());
        data.extend_from_slice(&amount.to_be_bytes::<32>());
        data.extend_from_slice(&min_amount_in.to_be_bytes::<32>());
        data.extend_from_slice(self.terms.nonce.as_slice());
        data.extend_from_slice(&self.terms.deadline.to_be_bytes()[2..]);
        data.extend_from_slice(&[0; SUBSIDY_SIGNATURE_LEN]);
        data.extend_from_slice(bytes_to_address(executor_address)?.as_slice());
        data.extend_from_slice(protocol_data);

        let subsidy = typed::Subsidy {
            boundTo: bound_to,
            token: convert_to_router_token(bytes_to_address(self.token_out)?),
            amount,
            minAmountIn: min_amount_in,
            innerSwapHash: keccak256(&data[SUBSIDY_SIGNATURE_OFFSET + SUBSIDY_SIGNATURE_LEN..]),
            nonce: U256::from_be_bytes(self.terms.nonce.0),
            deadline: U256::from(self.terms.deadline),
        };
        let domain = eip712_domain! {
            name: "TychoSubsidyPayer",
            version: "1",
            chain_id: self.encoder.chain.id(),
            verifying_contract: bytes_to_address(&self.encoder.payer_address)?,
        };
        let signature = SubsidizedGroupSignature {
            group_index: self.group_index,
            signing_hash: subsidy.eip712_signing_hash(&domain),
            signature_from_end: data.len() - SUBSIDY_SIGNATURE_OFFSET,
        };
        *executor_address = self.encoder.executor_address.clone();
        *protocol_data = data;
        Ok(signature)
    }
}

/// The solution's subsidy terms, or `None` when it has none or is cyclical.
pub fn encoded_subsidy(solution: &Solution) -> Option<&SubsidyTerms> {
    if solution.token_in() == solution.token_out() {
        return None;
    }
    solution.subsidy()
}

/// The swap group that carries the solution's subsidy, or `None` when [`encoded_subsidy`]
/// is `None`.
///
/// Expects a solution that passed [`validate_subsidy`]. Fails with `FatalError` when the chain
/// has no subsidizing executor.
pub fn select_subsidized_group<'a>(
    solution: &'a Solution,
    groups: &'a [SwapGroup],
    swap_encoder_registry: &'a SwapEncoderRegistry,
) -> Result<Option<SubsidizedGroup<'a>>, EncodingError> {
    let Some(terms) = encoded_subsidy(solution) else {
        return Ok(None);
    };
    let group_index = find_subsidized_group(solution, terms, groups)?;
    Ok(Some(SubsidizedGroup {
        group_index,
        token_out: &groups[group_index].token_out,
        terms,
        sender: solution.sender(),
        encoder: swap_encoder_registry.get_subsidy_encoder()?,
    }))
}

/// Fails unless the solution's subsidy, if any, can be paid: `InvalidInput` when the solution, its
/// subsidy terms or the swap group they name cannot carry it, `FatalError` when the chain has no
/// subsidizing executor.
pub fn validate_subsidy(
    solution: &Solution,
    groups: &[SwapGroup],
    swap_encoder_registry: &SwapEncoderRegistry,
) -> Result<(), EncodingError> {
    let Some(terms) = encoded_subsidy(solution) else {
        return Ok(());
    };
    let group_index = find_subsidized_group(solution, terms, groups)?;
    check_not_wrap(&groups[group_index])?;
    validate_terms(solution, terms)?;
    swap_encoder_registry.get_subsidy_encoder()?;
    Ok(())
}

/// Index of the group that holds the swap at `subsidized_swap_index`.
fn find_subsidized_group(
    solution: &Solution,
    terms: &SubsidyTerms,
    groups: &[SwapGroup],
) -> Result<usize, EncodingError> {
    let swap_index = terms.subsidized_swap_index;
    let mut swaps_before_group = 0;
    for (group_index, group) in groups.iter().enumerate() {
        if swap_index < swaps_before_group + group.swaps.len() {
            return Ok(group_index);
        }
        swaps_before_group += group.swaps.len();
    }
    Err(EncodingError::InvalidInput(format!(
        "Subsidy subsidized_swap_index {swap_index} is out of range for a solution with {} swaps",
        solution.swaps().len()
    )))
}

/// Fails with `InvalidInput` unless the solution and its subsidy terms can carry a subsidy.
fn validate_terms(solution: &Solution, terms: &SubsidyTerms) -> Result<(), EncodingError> {
    bytes_to_address(solution.sender())?;
    for (name, value) in [("amount", &terms.amount), ("min_amount_in", &terms.min_amount_in)] {
        if value.bits() > 256 {
            return Err(EncodingError::InvalidInput(format!(
                "Subsidy {name} {value} does not fit in 256 bits"
            )));
        }
    }
    if terms.deadline > MAX_ENCODABLE_DEADLINE {
        return Err(EncodingError::InvalidInput(format!(
            "Subsidy deadline {} does not fit in 48 bits",
            terms.deadline
        )));
    }
    Ok(())
}

/// Fails with `InvalidInput` when `group` is a wrap/unwrap, which cannot carry a subsidy.
fn check_not_wrap(group: &SwapGroup) -> Result<(), EncodingError> {
    if group.protocol_system == NATIVE_WRAPPER_PROTOCOL_SYSTEM {
        return Err(EncodingError::InvalidInput(
            "A wrap/unwrap cannot carry a subsidy: name a pool swap".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::group_swaps::group_swaps,
        models::{default_token, Swap},
    };

    const EXECUTOR: &str = "0x1111111111111111111111111111111111111111";
    const PAYER: &str = "0x3333333333333333333333333333333333333333";
    const INNER_EXECUTOR: &str = "0x4444444444444444444444444444444444444444";
    const SENDER: &str = "0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2";
    const ETH: &str = "0x0000000000000000000000000000000000000000";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn terms() -> SubsidyTerms {
        SubsidyTerms {
            amount: BigUint::from(5_000_000_000_000_000_000u64),
            min_amount_in: BigUint::from(1_000_000_000_000_000_000u64),
            nonce: B256::with_last_byte(7),
            deadline: 1_700_000_000,
            subsidized_swap_index: 0,
        }
    }

    fn swap(protocol_system: &str, token_in: &str, token_out: &str, split: f64) -> Swap {
        Swap::new(
            ProtocolComponent {
                protocol_system: protocol_system.to_string(),
                ..Default::default()
            },
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        )
        .with_split(split)
    }

    fn solution(
        token_in: &str,
        token_out: &str,
        swaps: Vec<Swap>,
        terms: SubsidyTerms,
    ) -> Solution {
        Solution::new(
            Bytes::from(SENDER),
            Bytes::from(SENDER),
            Bytes::from(token_in),
            Bytes::from(token_out),
            BigUint::from(1_000_000_000_000_000_000u64),
            BigUint::from(100_000_000_000_000_000_000u128),
            BigUint::from(95_000_000_000_000_000_000u128),
            swaps,
        )
        .with_subsidy(terms)
    }

    fn registry() -> SwapEncoderRegistry {
        SwapEncoderRegistry::new(Chain::Ethereum)
            .register_subsidy_contracts(Bytes::from(EXECUTOR), Bytes::from(PAYER))
    }

    /// The subsidizing executor, its protocol data and the signature of a wrapped group.
    fn wrap(token_out: &str) -> (Bytes, Vec<u8>, SubsidizedGroupSignature) {
        let encoder =
            SubsidyEncoder::new(Bytes::from(EXECUTOR), Bytes::from(PAYER), Chain::Ethereum);
        let token_out = Bytes::from(token_out);
        let terms = terms();
        let sender = Bytes::from(SENDER);
        let group = SubsidizedGroup {
            group_index: 0,
            token_out: &token_out,
            terms: &terms,
            sender: &sender,
            encoder: &encoder,
        };
        let mut executor_address = Bytes::from(INNER_EXECUTOR);
        let mut protocol_data = vec![0xcd; 3];
        let signature = group
            .wrap(&mut executor_address, &mut protocol_data)
            .unwrap();
        (executor_address, protocol_data, signature)
    }

    /// Expected hashes computed with `cast` from the payer's domain ("TychoSubsidyPayer", "1",
    /// chain 1, PAYER) and `keccak256(abi.encode(SUBSIDY_TYPEHASH, subsidy))`, with
    /// `innerSwapHash = keccak256(INNER_EXECUTOR ‖ 0xcdcdcd)`.
    #[rstest]
    #[case::erc20(DAI, "f7dd86d6179110244a35589b577bd51b2c8cabe0839e326ef4d7086f6784cf49")]
    #[case::native_eth_as_router_eth_address(
        ETH,
        "b74e75aeb431d41995665e46da05dd44fb74b57ccb25ef1be621b0a72193fafb"
    )]
    fn test_subsidy_signing_hash(#[case] token_out: &str, #[case] expected: &str) {
        let (_, _, signature) = wrap(token_out);

        assert_eq!(encode(signature.signing_hash), expected);
    }

    #[test]
    fn test_wrapper_data_layout() {
        let (executor_address, protocol_data, signature) = wrap(DAI);

        assert_eq!(executor_address, Bytes::from(EXECUTOR));
        assert_eq!(signature.signature_from_end, SUBSIDY_SIGNATURE_LEN + 20 + 3);
        assert_eq!(
            encode(protocol_data),
            [
                "cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2",
                "0000000000000000000000000000000000000000000000004563918244f40000",
                "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                "0000000000000000000000000000000000000000000000000000000000000007",
                "00006553f100",
                &"00".repeat(SUBSIDY_SIGNATURE_LEN),
                "4444444444444444444444444444444444444444",
                "cdcdcd",
            ]
            .concat()
        );
    }

    #[rstest]
    #[case::last_pool_swap(vec![swap("uniswap_v2", WETH, DAI, 0.0)], DAI, 0, 0)]
    #[case::pool_swap_before_final_unwrap(
        vec![swap("uniswap_v2", DAI, WETH, 0.0), swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, WETH, ETH, 0.0)],
        ETH,
        0,
        0
    )]
    #[case::pool_swap_before_final_wrap(
        vec![swap("uniswap_v4", USDC, ETH, 0.0), swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, ETH, WETH, 0.0)],
        WETH,
        0,
        0
    )]
    #[case::split_swap_into_unwrap(
        vec![
            swap("uniswap_v2", DAI, WETH, 0.7),
            swap("uniswap_v3", DAI, WETH, 0.0),
            swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, WETH, ETH, 0.0),
        ],
        ETH,
        0,
        0
    )]
    #[case::first_swap_of_a_multi_swap_group(
        vec![swap("uniswap_v4", USDC, WETH, 0.0), swap("uniswap_v4", WETH, DAI, 0.0)],
        DAI,
        0,
        0
    )]
    #[case::second_swap_of_a_multi_swap_group(
        vec![swap("uniswap_v4", USDC, WETH, 0.0), swap("uniswap_v4", WETH, DAI, 0.0)],
        DAI,
        1,
        0
    )]
    #[case::swap_after_a_multi_swap_group(
        vec![
            swap("uniswap_v4", USDC, WETH, 0.0),
            swap("uniswap_v4", WETH, DAI, 0.0),
            swap("uniswap_v2", DAI, ETH, 0.0),
        ],
        ETH,
        2,
        1
    )]
    fn test_select_subsidized_group(
        #[case] swaps: Vec<Swap>,
        #[case] token_out: &str,
        #[case] subsidized_swap_index: usize,
        #[case] expected: usize,
    ) {
        let token_in = swaps[0].token_in().address.to_string();
        let solution = solution(
            &token_in,
            token_out,
            swaps,
            SubsidyTerms { subsidized_swap_index, ..terms() },
        );
        let groups = group_swaps(solution.swaps());
        let registry = registry();

        let group = select_subsidized_group(&solution, &groups, &registry)
            .unwrap()
            .unwrap();

        assert_eq!(group.group_index, expected);
    }

    #[test]
    fn test_cyclical_solution_drops_subsidy() {
        let solution = solution(
            WETH,
            WETH,
            vec![swap("uniswap_v2", WETH, DAI, 0.0), swap("uniswap_v2", DAI, WETH, 0.0)],
            SubsidyTerms { subsidized_swap_index: 1, ..terms() },
        );
        let groups = group_swaps(solution.swaps());
        let registry = SwapEncoderRegistry::new(Chain::Ethereum);

        validate_subsidy(&solution, &groups, &registry).unwrap();
        let group = select_subsidized_group(&solution, &groups, &registry).unwrap();

        assert!(group.is_none());
    }

    fn sequential_weth_dai_usdc() -> Vec<Swap> {
        vec![swap("uniswap_v2", WETH, DAI, 0.0), swap("uniswap_v2", DAI, USDC, 0.0)]
    }

    #[rstest]
    #[case::subsidized_swap_index_out_of_range(
        WETH,
        USDC,
        sequential_weth_dai_usdc(),
        2,
        "out of range"
    )]
    #[case::wrap_unwrap_chosen(
        DAI,
        ETH,
        vec![swap("uniswap_v2", DAI, WETH, 0.0), swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, WETH, ETH, 0.0)],
        1,
        "wrap/unwrap cannot carry"
    )]
    #[case::output_does_not_reach_token_out(
        WETH,
        USDC,
        sequential_weth_dai_usdc(),
        0,
        "neither directly"
    )]
    #[case::unwrap_takes_part_of_the_token(
        DAI,
        ETH,
        vec![
            swap("uniswap_v2", DAI, WETH, 0.0),
            swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, WETH, ETH, 0.6),
            swap("uniswap_v4", WETH, ETH, 0.0),
        ],
        0,
        "takes only part"
    )]
    #[case::sole_unwrap_with_a_split(
        DAI,
        ETH,
        vec![swap("uniswap_v2", DAI, WETH, 0.0), swap(NATIVE_WRAPPER_PROTOCOL_SYSTEM, WETH, ETH, 0.6)],
        0,
        "takes only part"
    )]
    fn test_validate_subsidy_invalid_swap(
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] swaps: Vec<Swap>,
        #[case] subsidized_swap_index: usize,
        #[case] message: &str,
    ) {
        let solution =
            solution(token_in, token_out, swaps, SubsidyTerms { subsidized_swap_index, ..terms() });
        let groups = group_swaps(solution.swaps());
        let registry = registry();

        let result = validate_subsidy(&solution, &groups, &registry);

        assert!(
            matches!(result, Err(EncodingError::InvalidInput(ref error)) if error.contains(message)),
            "{:?}",
            result.err()
        );
    }

    #[rstest]
    #[case::deadline_over_48_bits(SubsidyTerms { deadline: 1 << 48, ..terms() }, "48 bits")]
    #[case::amount_over_256_bits(
        SubsidyTerms { amount: BigUint::from(1u32) << 256, ..terms() },
        "Subsidy amount"
    )]
    #[case::min_amount_in_over_256_bits(
        SubsidyTerms { min_amount_in: BigUint::from(1u32) << 256, ..terms() },
        "Subsidy min_amount_in"
    )]
    #[case::min_amount_out_includes_subsidy(
        SubsidyTerms { amount: BigUint::from(5_000_000_000_000_000_001u64), ..terms() },
        "must be at most"
    )]
    fn test_validate_subsidy_invalid_terms(#[case] terms: SubsidyTerms, #[case] message: &str) {
        let solution = solution(WETH, DAI, vec![swap("uniswap_v2", WETH, DAI, 0.0)], terms);
        let groups = group_swaps(solution.swaps());
        let registry = registry();

        let result = validate_subsidy(&solution, &groups, &registry);

        assert!(
            matches!(result, Err(EncodingError::InvalidInput(ref error)) if error.contains(message)),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_subsidy_without_subsidizing_executor() {
        let solution = solution(WETH, DAI, vec![swap("uniswap_v2", WETH, DAI, 0.0)], terms());
        let groups = group_swaps(solution.swaps());
        let registry = SwapEncoderRegistry::new(Chain::Ethereum);

        let result = validate_subsidy(&solution, &groups, &registry);

        assert!(
            matches!(result, Err(EncodingError::FatalError(ref error)) if error.contains("No subsidizing executor")),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_subsidy_invalid_sender() {
        let solution = solution(WETH, DAI, vec![swap("uniswap_v2", WETH, DAI, 0.0)], terms());
        let solution = Solution::new(
            Bytes::from("0x1234"),
            solution.receiver().clone(),
            solution.token_in().clone(),
            solution.token_out().clone(),
            solution.amount_in().clone(),
            solution.expected_amount_out().clone(),
            solution.min_amount_out().clone(),
            solution.swaps().to_vec(),
        )
        .with_subsidy(terms());
        let groups = group_swaps(solution.swaps());
        let registry = registry();

        let result = validate_subsidy(&solution, &groups, &registry);

        assert!(
            matches!(result, Err(EncodingError::InvalidInput(ref error)) if error.contains("Invalid address")),
            "{:?}",
            result.err()
        );
    }

    fn payer_config(payer: &str) -> HashMap<String, String> {
        HashMap::from([(PAYER_CONFIG_KEY.to_string(), payer.to_string())])
    }

    #[test]
    fn test_from_config() {
        let encoder = SubsidyEncoder::from_config(
            Bytes::from(EXECUTOR),
            Chain::Ethereum,
            Some(&payer_config(PAYER)),
        )
        .unwrap();

        assert_eq!(encoder.payer_address, Bytes::from(PAYER));
        assert_eq!(encoder.executor_address, Bytes::from(EXECUTOR));
    }

    #[rstest]
    #[case::missing_config(None, "missing")]
    #[case::not_hex(Some(payer_config("not an address")), "Invalid subsidy payer_address")]
    #[case::not_20_bytes(Some(payer_config("0x1234")), "Invalid subsidy payer_address")]
    fn test_from_config_invalid_payer(
        #[case] config: Option<HashMap<String, String>>,
        #[case] message: &str,
    ) {
        let result =
            SubsidyEncoder::from_config(Bytes::from(EXECUTOR), Chain::Ethereum, config.as_ref());

        assert!(
            matches!(result, Err(EncodingError::FatalError(ref error)) if error.contains(message)),
            "{result:?}"
        );
    }
}
