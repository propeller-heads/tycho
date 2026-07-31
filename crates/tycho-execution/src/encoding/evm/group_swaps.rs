use num_bigint::BigUint;
use tycho_common::Bytes;

use crate::encoding::{evm::constants::GROUPABLE_PROTOCOLS, models::Swap};

/// Represents a group of swaps that can be encoded into a single swap execution for gas
/// optimization.
///
/// # Fields
/// * `token_in`: Bytes, the input token of the first swap
/// * `token_out`: Bytes, the output token of the final swap
/// * `protocol_system`: String, the protocol system of the swaps
/// * `swaps`: Vec<Swap>, the sequence of swaps to be executed as a group
/// * `split`: f64, the split percentage of the first swap in the group
/// * `estimated_gas`: BigUint, the estimated gas usage of the swap group
#[derive(Clone, Debug)]
pub struct SwapGroup {
    pub token_in: Bytes,
    pub token_out: Bytes,
    pub protocol_system: String,
    pub swaps: Vec<Swap>,
    pub split: f64,
    pub estimated_gas: BigUint,
}

impl PartialEq for SwapGroup {
    fn eq(&self, other: &Self) -> bool {
        self.token_in == other.token_in &&
            self.token_out == other.token_out &&
            self.protocol_system == other.protocol_system &&
            self.swaps == other.swaps &&
            self.split == other.split &&
            self.estimated_gas == other.estimated_gas
    }
}

/// Group consecutive swaps which can be encoded into one swap execution for gas optimization.
///
/// An example where this applies is the case of USV4, which uses a PoolManager contract
/// to save token transfers on consecutive swaps.
pub fn group_swaps(swaps: &[Swap]) -> Vec<SwapGroup> {
    let mut grouped_swaps: Vec<SwapGroup> = Vec::new();
    let mut current_group: Option<SwapGroup> = None;
    let mut last_swap_protocol = "".to_string();
    let mut groupable_protocol;
    let mut last_swap_out_token = Bytes::default();
    for swap in swaps {
        let mut current_swap_protocol = swap.component().protocol_system.clone();
        // Normalize uniswap_v4_hooks to uniswap_v4 for grouping (same PoolManager)
        if current_swap_protocol == "uniswap_v4_hooks" {
            current_swap_protocol = "uniswap_v4".to_string();
        };
        groupable_protocol = GROUPABLE_PROTOCOLS.contains(&current_swap_protocol.as_str());

        // Split 0 can also mean that the swap is the remaining part of a branch of splits,
        // so we need to check the last swap's out token as well
        let no_split = swap.split() == 0.0 && swap.token_in().address == last_swap_out_token;

        // Merging this swap would make the group's token_out equal to its token_in, which
        // the router rejects as an unsupported single-hop cycle. Keep the swap in its
        // own group instead.
        let no_cycle = current_group
            .as_ref()
            .is_none_or(|g| swap.token_out().address != g.token_in);

        if current_swap_protocol == last_swap_protocol && groupable_protocol && no_split && no_cycle
        {
            // Second or later groupable pool in a sequence of groupable pools. Merge to the
            // current group.
            if let Some(group) = current_group.as_mut() {
                group.swaps.push(swap.clone());
                // Update the output token of the current group.
                group.token_out = swap.token_out().address.clone();
            }
        } else {
            // Not second or later USV4 pool. Push the current group (if it exists) and then
            // create a new group.
            if let Some(mut group) = current_group.take() {
                group.estimated_gas = compute_group_gas(&group.swaps);
                grouped_swaps.push(group);
            }
            current_group = Some(SwapGroup {
                token_in: swap.token_in().address.clone(),
                token_out: swap.token_out().address.clone(),
                protocol_system: current_swap_protocol.clone(),
                swaps: vec![swap.clone()],
                split: swap.split(),
                estimated_gas: BigUint::ZERO,
            });
        }
        last_swap_protocol = current_swap_protocol;
        last_swap_out_token = swap.token_out().address.clone();
    }
    if let Some(mut group) = current_group.take() {
        group.estimated_gas = compute_group_gas(&group.swaps);
        grouped_swaps.push(group);
    }
    grouped_swaps
}

/// Aggregate per-swap gas estimates for a group, discounting transfers skipped by batching.
///
/// Each swap's `estimated_gas` is assumed to include its input and output token transfer
/// costs (this is acceptable because all protocols that can group do so with a callback mechanism).
/// When swaps are batched (e.g. USV4's PoolManager flash accounting), intermediate
/// transfers are saved: the first swap's output transfer, every middle swap's input and
/// output transfers, and the last swap's input transfer. The group's external input and
/// output transfers remain.
fn compute_group_gas(swaps: &[Swap]) -> BigUint {
    let mut total_gas: BigUint = swaps
        .iter()
        .map(|s| s.estimated_gas().clone())
        .sum();
    let n = swaps.len();
    if n <= 1 {
        return total_gas;
    }
    let safe_sub = |t: BigUint, v: BigUint| if t >= v { t - v } else { BigUint::ZERO };
    total_gas = safe_sub(total_gas, swaps[0].token_out().gas_usage());
    for swap in &swaps[1..n - 1] {
        total_gas = safe_sub(total_gas, swap.token_in().gas_usage());
        total_gas = safe_sub(total_gas, swap.token_out().gas_usage());
    }
    total_gas = safe_sub(total_gas, swaps[n - 1].token_in().gas_usage());
    total_gas
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::hex;
    use tycho_common::{
        models::{protocol::ProtocolComponent, token::Token},
        Bytes,
    };

    use super::*;
    use crate::encoding::models::{default_token, Swap};

    fn weth() -> Bytes {
        Bytes::from(hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").to_vec())
    }

    fn token_with_gas(address: Bytes, gas: u64) -> Token {
        Token::new(&address, "", 0, 0, &[Some(gas)], Default::default(), 100)
    }

    #[test]
    fn test_group_swaps_simple() {
        // The first and second swaps can be grouped since there is no split, and they are
        // both USV4.
        //
        //   WETH ──(USV4)──> WBTC ───(USV4)──> USDC ───(USV2)──> DAI

        let weth = weth();
        let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let swap_weth_wbtc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(wbtc.clone()),
            BigUint::from(200_000u64),
        );

        let swap_wbtc_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(wbtc.clone()),
            default_token(usdc.clone()),
            BigUint::from(220_000u64),
        );

        let swap_usdc_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v2".to_string(), ..Default::default() },
            default_token(usdc.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        );
        let swaps = vec![swap_weth_wbtc.clone(), swap_wbtc_usdc.clone(), swap_usdc_dai.clone()];
        let grouped_swaps = group_swaps(&swaps);

        assert_eq!(
            grouped_swaps,
            vec![
                SwapGroup {
                    swaps: vec![swap_weth_wbtc, swap_wbtc_usdc],
                    token_in: weth,
                    token_out: usdc.clone(),
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(300_000u64),
                },
                SwapGroup {
                    swaps: vec![swap_usdc_dai],
                    token_in: usdc,
                    token_out: dai,
                    protocol_system: "uniswap_v2".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::ZERO,
                }
            ]
        );
    }

    #[test]
    fn test_group_swaps_complex_split() {
        // There is a split in the solution, but it's possible to combine two of the USV4 splits.
        // The WETH -> USDC swap cannot get grouped with anything, but the WETH -> DAI and
        // DAI -> USDC swaps can be grouped.
        //
        //                            ┌──(USV4)──> USDC
        //   WBTC ──> (USV4)──> WETH ─┤
        //                            └──(USV4)──> DAI ───(USV4)──> USDC

        let weth = weth();
        let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let swap_wbtc_weth = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(wbtc.clone()),
            default_token(weth.clone()),
            BigUint::ZERO,
        );
        let swap_weth_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        )
        .with_split(0.5f64);
        let swap_weth_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::from(220_000u64),
        );
        // Split 0 represents the remaining 50%, but to avoid any rounding errors we set this to
        // 0 to signify "the remainder of the WETH value". It should still be very close to 50%

        let swap_dai_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(dai.clone()),
            default_token(usdc.clone()),
            BigUint::from(250_000u64),
        );
        let swaps = vec![
            swap_wbtc_weth.clone(),
            swap_weth_usdc.clone(),
            swap_weth_dai.clone(),
            swap_dai_usdc.clone(),
        ];
        let grouped_swaps = group_swaps(&swaps);

        assert_eq!(
            grouped_swaps,
            vec![
                SwapGroup {
                    swaps: vec![swap_wbtc_weth],
                    token_in: wbtc.clone(),
                    token_out: weth.clone(),
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::ZERO,
                },
                SwapGroup {
                    swaps: vec![swap_weth_usdc],
                    token_in: weth.clone(),
                    token_out: usdc.clone(),
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0.5f64,
                    estimated_gas: BigUint::ZERO,
                },
                SwapGroup {
                    swaps: vec![swap_weth_dai, swap_dai_usdc],
                    token_in: weth,
                    token_out: usdc,
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(350_000u64),
                }
            ]
        );
    }

    #[test]
    fn test_group_swaps_complex_split_multi_protocol() {
        // There is a split in the solution, but it's possible to group the USV4 splits with each
        // other and the Balancer V3 swaps with each other.
        //
        //         ┌──(BalancerV3)──> WBTC ──(BalancerV3)──> USDC
        //   WETH ─┤
        //         └──(USV4)──> DAI ───(USV4)──> USDC

        let weth = weth();
        let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let swap_weth_wbtc = Swap::new(
            ProtocolComponent {
                protocol_system: "vm:balancer_v3".to_string(),
                ..Default::default()
            },
            default_token(weth.clone()),
            default_token(wbtc.clone()),
            BigUint::from(220_000u64),
        )
        .with_split(0.5f64);

        let swap_wbtc_usdc = Swap::new(
            ProtocolComponent {
                protocol_system: "vm:balancer_v3".to_string(),
                ..Default::default()
            },
            default_token(wbtc.clone()),
            default_token(usdc.clone()),
            BigUint::from(220_000u64),
        );
        let swap_weth_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::from(250_000u64),
        );
        let swap_dai_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(dai.clone()),
            default_token(usdc.clone()),
            BigUint::from(250_000u64),
        );

        let swaps = vec![
            swap_weth_wbtc.clone(),
            swap_wbtc_usdc.clone(),
            swap_weth_dai.clone(),
            swap_dai_usdc.clone(),
        ];
        let grouped_swaps = group_swaps(&swaps);

        assert_eq!(
            grouped_swaps,
            vec![
                SwapGroup {
                    swaps: vec![swap_weth_wbtc, swap_wbtc_usdc],
                    token_in: weth.clone(),
                    token_out: usdc.clone(),
                    protocol_system: "vm:balancer_v3".to_string(),
                    split: 0.5f64,
                    estimated_gas: BigUint::from(320_000u64),
                },
                SwapGroup {
                    swaps: vec![swap_weth_dai, swap_dai_usdc],
                    token_in: weth,
                    token_out: usdc,
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(380_000u64),
                }
            ]
        );
    }

    #[test]
    fn test_group_swaps_cyclic_two_hops() {
        // A two-hop cycle entirely on USV4 must NOT be collapsed into one group,
        // because that would produce token_in == token_out, which
        // the Router rejects as an unsupported single-hop cycle.
        //
        //   USDC ──(USV4)──> WETH ──(USV4)──> USDC
        //
        // Expected: two separate groups
        //   Group 1: USDC -> WETH
        //   Group 2: WETH -> USDC

        let weth = weth();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        let swap_usdc_weth = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(usdc.clone()),
            default_token(weth.clone()),
            BigUint::from(220_000u64),
        );
        let swap_weth_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(usdc.clone()),
            BigUint::from(220_000u64),
        );

        let grouped_swaps = group_swaps(&[swap_usdc_weth.clone(), swap_weth_usdc.clone()]);

        assert_eq!(
            grouped_swaps,
            vec![
                SwapGroup {
                    swaps: vec![swap_usdc_weth],
                    token_in: usdc.clone(),
                    token_out: weth.clone(),
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(220_000u64),
                },
                SwapGroup {
                    swaps: vec![swap_weth_usdc],
                    token_in: weth,
                    token_out: usdc,
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(220_000u64),
                },
            ]
        );
    }

    #[test]
    fn test_group_swaps_cyclic_three_hops() {
        // A three-hop cycle on USV4: the first two hops are safe to group.
        // The third hop would make the group cyclic, so it must start a new group.
        //
        //   USDC ──(USV4)──> WETH ──(USV4)──> DAI ──(USV4)──> USDC
        //
        // Expected: two groups
        //   Group 1: USDC -> WETH -> DAI  (two swaps)
        //   Group 2: DAI  -> USDC        (one swap)

        let weth = weth();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let swap_usdc_weth = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(usdc.clone()),
            default_token(weth.clone()),
            BigUint::from(220_000u64),
        );
        let swap_weth_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::from(250_000u64),
        );
        let swap_dai_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(dai.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        );

        let grouped_swaps =
            group_swaps(&[swap_usdc_weth.clone(), swap_weth_dai.clone(), swap_dai_usdc.clone()]);

        assert_eq!(
            grouped_swaps,
            vec![
                SwapGroup {
                    swaps: vec![swap_usdc_weth, swap_weth_dai],
                    token_in: usdc.clone(),
                    token_out: dai.clone(),
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::from(350_000u64),
                },
                SwapGroup {
                    swaps: vec![swap_dai_usdc],
                    token_in: dai,
                    token_out: usdc,
                    protocol_system: "uniswap_v4".to_string(),
                    split: 0f64,
                    estimated_gas: BigUint::ZERO,
                },
            ]
        );
    }

    #[test]
    fn test_group_swaps_uniswap_v4_with_hooks() {
        // Test that uniswap_v4 and uniswap_v4_hooks can be grouped together
        // since they use the same PoolManager and flash accounting system.
        //
        //   WETH ──(USV4)──> WBTC ───(USV4_hooks)──> USDC ───(USV2)──> DAI

        let weth = weth();
        let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let dai = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        let swap_weth_wbtc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            default_token(weth.clone()),
            default_token(wbtc.clone()),
            BigUint::ZERO,
        );

        let swap_wbtc_usdc = Swap::new(
            ProtocolComponent {
                protocol_system: "uniswap_v4_hooks".to_string(),
                ..Default::default()
            },
            default_token(wbtc.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        );

        let swap_usdc_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v2".to_string(), ..Default::default() },
            default_token(usdc.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        );
        let swaps = vec![swap_weth_wbtc.clone(), swap_wbtc_usdc.clone(), swap_usdc_dai.clone()];
        let grouped_swaps = group_swaps(&swaps);

        assert_eq!(grouped_swaps.len(), 2);
        // First group should contain both uniswap_v4 and uniswap_v4_hooks swaps
        assert_eq!(grouped_swaps[0].swaps.len(), 2);
        assert_eq!(grouped_swaps[0].token_in, weth);
        assert_eq!(grouped_swaps[0].token_out, usdc.clone());
        // The protocol_system should be from the first swap in the group
        assert_eq!(grouped_swaps[0].protocol_system, "uniswap_v4");

        // Second group should be the uniswap_v2 swap
        assert_eq!(grouped_swaps[1].swaps.len(), 1);
        assert_eq!(grouped_swaps[1].token_in, usdc);
        assert_eq!(grouped_swaps[1].token_out, dai);
        assert_eq!(grouped_swaps[1].protocol_system, "uniswap_v2");
    }

    #[test]
    fn test_group_swaps_estimated_gas() {
        //   WETH ──(USV4)──> WBTC ──(USV4)──> USDC ──(USV4)──> DAI

        let weth = token_with_gas(weth(), 10);
        let wbtc = token_with_gas(
            Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap(),
            20,
        );
        let usdc = token_with_gas(
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            30,
        );
        let dai = token_with_gas(
            Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            40,
        );

        let swap_weth_wbtc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            weth,
            wbtc.clone(),
            BigUint::from(1000u64),
        );
        let swap_wbtc_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            wbtc,
            usdc.clone(),
            BigUint::from(1500u64),
        );
        let swap_usdc_dai = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            usdc,
            dai,
            BigUint::from(2000u64),
        );

        let grouped_swaps = group_swaps(&[swap_weth_wbtc, swap_wbtc_usdc, swap_usdc_dai]);

        assert_eq!(grouped_swaps.len(), 1);
        // Sum: 1000 + 1500 + 2000 = 4500
        // - first.token_out (wbtc=20) = 4480
        // - middle.token_in (wbtc=20) + middle.token_out (usdc=30) = 4430
        // - last.token_in (usdc=30) = 4400
        assert_eq!(grouped_swaps[0].estimated_gas, BigUint::from(4400u64));
    }

    #[test]
    fn test_group_swaps_estimated_gas_single_swap_group() {
        // A single-swap group has no batching benefit, so its estimated_gas equals the
        // swap's own estimated_gas with no subtraction.
        let weth = token_with_gas(weth(), 10);
        let usdc = token_with_gas(
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            30,
        );

        let swap = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v2".to_string(), ..Default::default() },
            weth,
            usdc,
            BigUint::from(1234u64),
        );

        let grouped_swaps = group_swaps(&[swap]);

        assert_eq!(grouped_swaps.len(), 1);
        assert_eq!(grouped_swaps[0].estimated_gas, BigUint::from(1234u64));
    }

    #[test]
    fn test_group_swaps_estimated_gas_saturates_to_zero() {
        // If the per-swap gas estimates are smaller than the transfer costs we need to
        // subtract, the result should saturate to zero rather than underflow.
        // This should never happen though!!
        let weth = token_with_gas(weth(), 10_000);
        let wbtc = token_with_gas(
            Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap(),
            10_000,
        );
        let usdc = token_with_gas(
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            10_000,
        );

        let swap_weth_wbtc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            weth,
            wbtc.clone(),
            BigUint::from(50u64),
        );
        let swap_wbtc_usdc = Swap::new(
            ProtocolComponent { protocol_system: "uniswap_v4".to_string(), ..Default::default() },
            wbtc,
            usdc,
            BigUint::from(50u64),
        );

        let grouped_swaps = group_swaps(&[swap_weth_wbtc, swap_wbtc_usdc]);

        assert_eq!(grouped_swaps.len(), 1);
        assert_eq!(grouped_swaps[0].estimated_gas, BigUint::ZERO);
    }
}
