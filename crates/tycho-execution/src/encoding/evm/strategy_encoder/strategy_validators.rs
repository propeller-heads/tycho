use std::collections::{HashMap, HashSet, VecDeque};

use tycho_common::Bytes;

use crate::encoding::{errors::EncodingError, models::Swap};

pub trait SwapValidator {
    /// Raises an error if swaps do not represent a valid path from the given token to the checked
    /// token.
    ///
    /// A path is considered valid if all the following conditions are met:
    /// * The checked token is reachable from the given token through the swap path
    /// * There are no tokens which are unconnected from the main path
    fn validate_swap_path(
        &self,
        swaps: &[Swap],
        given_token: &Bytes,
        checked_token: &Bytes,
    ) -> Result<(), EncodingError> {
        // Build directed graph of token flows
        let mut graph: HashMap<&Bytes, HashSet<&Bytes>> = HashMap::new();
        let mut all_tokens = HashSet::new();
        for swap in swaps {
            graph
                .entry(&swap.token_in().address)
                .or_default()
                .insert(&swap.token_out().address);
            all_tokens.insert(&swap.token_in().address);
            all_tokens.insert(&swap.token_out().address);
        }

        // BFS from validation_given
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(given_token);

        while let Some(token) = queue.pop_front() {
            if !visited.insert(token) {
                continue;
            }

            // Early success check - if we've reached the checked token and visited all tokens
            if token == checked_token && visited.len() == all_tokens.len() {
                return Ok(());
            }

            if let Some(next_tokens) = graph.get(token) {
                for &next_token in next_tokens {
                    if !visited.contains(next_token) {
                        queue.push_back(next_token);
                    }
                }
            }
        }

        // After BFS completes, check if both conditions are met:
        // 1. The checked token is in the visited set
        // 2. All unique tokens from the swaps are visited
        if visited.contains(checked_token) && visited.len() == all_tokens.len() {
            return Ok(());
        }

        // If we get here, either checked_token wasn't reached or not all tokens were visited
        if !visited.contains(checked_token) {
            Err(EncodingError::InvalidInput(
                "Checked token is not reachable through swap path".to_string(),
            ))
        } else {
            Err(EncodingError::InvalidInput(
                "Some tokens are not connected to the main path".to_string(),
            ))
        }
    }
}

/// Validates whether a single swap represents a valid solution.
#[derive(Clone)]
pub struct SingleSwapValidator;

impl SwapValidator for SingleSwapValidator {}

/// Validates whether a sequence of split swaps represents a valid solution.
#[derive(Clone)]
pub struct SplitSwapValidator;

impl SwapValidator for SplitSwapValidator {}

impl SplitSwapValidator {
    /// Raises an error if the split percentages are invalid.
    ///
    /// Split percentages are considered valid if all the following conditions are met:
    /// * Each split amount is < 1 (100%)
    /// * There is exactly one 0% split for each token, and it's the last swap specified, signifying
    ///   to the router to send the remainder of the token to the designated protocol
    /// * The sum of all non-remainder splits for each token is < 1 (100%)
    /// * There are no negative split amounts
    pub fn validate_split_percentages(&self, swaps: &[Swap]) -> Result<(), EncodingError> {
        let mut swaps_by_token: HashMap<&Bytes, Vec<&Swap>> = HashMap::new();
        for swap in swaps {
            if swap.split() >= 1.0 {
                return Err(EncodingError::InvalidInput(format!(
                    "Split percentage must be less than 1 (100%), got {}",
                    swap.split()
                )));
            }
            swaps_by_token
                .entry(&swap.token_in().address)
                .or_default()
                .push(swap);
        }

        for (token, token_swaps) in swaps_by_token {
            // Single swaps don't need remainder handling
            if token_swaps.len() == 1 {
                if token_swaps[0].split() != 0.0 {
                    return Err(EncodingError::InvalidInput(format!(
                        "Single swap must have 0% split for token {token}",
                    )));
                }
                continue;
            }

            let mut found_zero_split = false;
            let mut total_percentage = 0.0;
            for (i, swap) in token_swaps.iter().enumerate() {
                match (swap.split() == 0.0, i == token_swaps.len() - 1) {
                    (true, false) => {
                        return Err(EncodingError::InvalidInput(format!(
                            "The 0% split for token {token} must be the last swap",
                        )))
                    }
                    (true, true) => found_zero_split = true,
                    (false, _) => {
                        if swap.split() < 0.0 {
                            return Err(EncodingError::InvalidInput(format!(
                                "All splits must be >= 0% for token {token}"
                            )));
                        }
                        total_percentage += swap.split();
                    }
                }
            }

            if !found_zero_split {
                return Err(EncodingError::InvalidInput(format!(
                    "Token {token} must have exactly one 0% split for remainder handling"
                )));
            }

            // Total must be <100% to leave room for remainder
            if total_percentage >= 1.0 {
                return Err(EncodingError::InvalidInput(format!(
                    "Total of non-remainder splits for token {:?} must be <100%, got {}%",
                    token,
                    total_percentage * 100.0
                )));
            }
        }

        Ok(())
    }
}

/// Protocols whose venue is the next hop's `fundsExpectedAddress` and rejects itself as a swap
/// recipient, so a sequential route through the same component twice in a row always reverts.
const SELF_RECIPIENT_REJECTING_PROTOCOLS: &[&str] = &["biconomy"];

/// Validates whether a sequence of sequential swaps represents a valid solution.
#[derive(Clone)]
pub struct SequentialSwapValidator;

impl SwapValidator for SequentialSwapValidator {}

impl SequentialSwapValidator {
    /// Raises an error if two consecutive swaps run on the same component of a protocol whose
    /// venue rejects itself as recipient: the router would pay the first hop's output to the
    /// venue (the second hop's `fundsExpectedAddress`) and the venue would revert.
    pub fn validate_no_back_to_back_venue_hops(&self, swaps: &[Swap]) -> Result<(), EncodingError> {
        for pair in swaps.windows(2) {
            let (first, second) = (pair[0].component(), pair[1].component());
            if first.id == second.id &&
                SELF_RECIPIENT_REJECTING_PROTOCOLS.contains(&first.protocol_system.as_str())
            {
                return Err(EncodingError::InvalidInput(format!(
                    "Consecutive swaps on {} component {} always revert: the venue cannot \
                     receive the first hop's output",
                    first.protocol_system, first.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use tycho_common::{models::protocol::ProtocolComponent, Bytes};

    use super::*;
    use crate::encoding::models::{default_token, Swap};

    #[test]
    fn test_validate_path_single_swap() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let swaps = vec![Swap::new(
            ProtocolComponent {
                id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                protocol_system: "uniswap_v2".to_string(),
                ..Default::default()
            },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        )];
        let result = validator.validate_swap_path(&swaps, &weth, &dai);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_validate_path_multiple_swaps() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.5f64),
            Swap::new(
                ProtocolComponent {
                    id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(dai.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            ),
        ];
        let result = validator.validate_swap_path(&swaps, &weth, &usdc);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_validate_path_disconnected() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();

        let disconnected_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.5f64),
            // This swap is disconnected from the WETH->DAI path
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(wbtc.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            ),
        ];
        let result = validator.validate_swap_path(&disconnected_swaps, &weth, &usdc);
        assert!(matches!(
            result,
            Err(EncodingError::InvalidInput(msg)) if msg.contains("not reachable through swap path")
        ));
    }

    #[test]
    fn test_validate_path_cyclic_swap() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        let cyclic_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(usdc.clone()),
                default_token(weth.clone()),
                BigUint::ZERO,
            ),
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(usdc.clone()),
                BigUint::ZERO,
            ),
        ];

        // Test with USDC as both given token and checked token
        let result = validator.validate_swap_path(&cyclic_swaps, &usdc, &usdc);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_validate_path_unreachable_checked_token() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        let unreachable_swaps = vec![Swap::new(
            ProtocolComponent {
                id: "pool1".to_string(),
                protocol_system: "uniswap_v2".to_string(),
                ..Default::default()
            },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        )
        .with_split(1.0)];
        let result = validator.validate_swap_path(&unreachable_swaps, &weth, &usdc);
        assert!(matches!(
            result,
            Err(EncodingError::InvalidInput(msg)) if msg.contains("not reachable through swap path")
        ));
    }

    #[test]
    fn test_validate_path_empty_swaps() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        let empty_swaps: Vec<Swap> = vec![];
        let result = validator.validate_swap_path(&empty_swaps, &weth, &usdc);
        assert!(matches!(
            result,
            Err(EncodingError::InvalidInput(msg)) if msg.contains("not reachable through swap path")
        ));
    }

    #[test]
    fn test_validate_swap_single() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let swaps = vec![Swap::new(
            ProtocolComponent {
                id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
                protocol_system: "uniswap_v2".to_string(),
                ..Default::default()
            },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        )];
        let result = validator.validate_split_percentages(&swaps);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_validate_swaps_multiple() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        // Valid case: Multiple swaps with proper splits (50%, 30%, remainder)
        let valid_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.5),
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.3),
            Swap::new(
                ProtocolComponent {
                    id: "pool3".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            ),
        ];
        assert!(validator
            .validate_split_percentages(&valid_swaps)
            .is_ok());
    }

    #[test]
    fn test_validate_swaps_no_remainder_split() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let invalid_total_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.7),
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.3),
        ];
        assert!(matches!(
            validator.validate_split_percentages(&invalid_total_swaps),
            Err(EncodingError::InvalidInput(msg)) if msg.contains("must have exactly one 0% split")
        ));
    }

    #[test]
    fn test_validate_swaps_zero_split_not_at_end() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let invalid_zero_position_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            ),
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.5),
        ];
        assert!(matches!(
            validator.validate_split_percentages(&invalid_zero_position_swaps),
            Err(EncodingError::InvalidInput(msg)) if msg.contains("must be the last swap")
        ));
    }

    #[test]
    fn test_validate_swaps_splits_exceed_hundred_percent() {
        let validator = SplitSwapValidator;
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let invalid_overflow_swaps = vec![
            Swap::new(
                ProtocolComponent {
                    id: "pool1".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.6),
            Swap::new(
                ProtocolComponent {
                    id: "pool2".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            )
            .with_split(0.5),
            Swap::new(
                ProtocolComponent {
                    id: "pool3".to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    ..Default::default()
                },
                default_token(weth.clone()),
                default_token(dai.clone()),
                BigUint::ZERO,
            ),
        ];
        assert!(matches!(
            validator.validate_split_percentages(&invalid_overflow_swaps),
            Err(EncodingError::InvalidInput(msg)) if msg.contains("must be <100%")
        ));
    }

    #[test]
    fn test_validate_back_to_back_venue_hops() {
        let weth = Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap();
        let usdc = Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap();
        let cbbtc = Bytes::from_str("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf").unwrap();
        let hop = |id: &str, protocol_system: &str, token_in: &Bytes, token_out: &Bytes| {
            Swap::new(
                ProtocolComponent {
                    id: id.to_string(),
                    protocol_system: protocol_system.to_string(),
                    ..Default::default()
                },
                default_token(token_in.clone()),
                default_token(token_out.clone()),
                BigUint::ZERO,
            )
        };
        let venue = "0x0000009e5f98cdb4dd1e1d2e15b220217c88b3b8";
        let validator = SequentialSwapValidator;

        let twice = [hop(venue, "biconomy", &weth, &usdc), hop(venue, "biconomy", &usdc, &cbbtc)];
        assert!(matches!(
            validator.validate_no_back_to_back_venue_hops(&twice),
            Err(EncodingError::InvalidInput(_))
        ));

        let via_other_pool = [
            hop(venue, "biconomy", &weth, &usdc),
            hop("pool", "uniswap_v3", &usdc, &weth),
            hop(venue, "biconomy", &weth, &cbbtc),
        ];
        assert!(validator
            .validate_no_back_to_back_venue_hops(&via_other_pool)
            .is_ok());
    }
}
