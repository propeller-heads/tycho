use num_bigint::BigUint;

use crate::encoding::models::{Solution, Strategy, UserTransferType};

/// Default gas usage for an ERC-20 `transferFrom` or `transfer`. Used as fallback when the token
/// has no measured gas usage.
pub const DEFAULT_TOKEN_TRANSFER_GAS: u64 = 40_000;

/// Gas usage for an ERC-20 `approve`.
pub const TOKEN_APPROVAL_GAS: u64 = 25_000;

/// Gas usage for a permit2 transfer from the user
pub const USER_PERMIT2_TRANSFER: u64 = 80_000;

/// Callback-based protocols: the input `transferFrom` happens inside the callback, which is
/// inside `swap()`. The gas is thus already included in `get_amount_out`.
pub const PROTOCOLS_CALLBACK: &[&str] = &[
    "uniswap_v3",
    "pancakeswap_v3",
    "uniswap_v4",
    "uniswap_v4_hooks",
    "ekubo_v2",
    "ekubo_v3",
    "aerodrome_slipstreams",
    "velodrome_slipstreams",
    "vm:balancer_v3",
];

/// Protocols where the router-to-pool input transfer is skipped (funds are sent directly without
/// an intermediate router hop). The input transfer cost is therefore not double-counted — it is
/// only charged once, on the first hop or when the optimized path applies.
pub const PROTOCOLS_OPTIMIZABLE_TRANSFER_IN: &[&str] =
    &["erc4626", "maverick_v2", "uniswap_v2", "sushiswap_v2", "pancakeswap_v2", "quickswap_v2"];

/// ProtocolWillDebit: the router must `approve(protocol)` before swapping.
/// The protocol's `transferFrom` is inside `swap()` and already in the gas computation of
/// `get_amount_out`, but the approval is not.
pub const PROTOCOLS_NEEDING_APPROVAL: &[&str] =
    &["vm:balancer_v2", "vm:curve", "rfq:bebop", "rfq:hashflow", "rfq:liquorice", "erc4626"];

/// `outputToRouter = true`: the pool sends output to the router, which then does an extra
/// `_transferOut` to the receiver.
pub const PROTOCOLS_OUTPUT_TO_ROUTER: &[&str] =
    &["vm:curve", "rocketpool", "fluid_v1", "native_wrapper"];

pub const ROUTER_FEES_ACTIVE: bool = true;

/// Estimates the total gas cost for executing a `Solution`.
///
/// Sums, for every swap in the solution:
///
/// - **Pool gas** (`swap.estimated_gas()`): the simulation-reported cost of the protocol's `swap()`
///   call, which might already include any transfers the pool performs internally (see
///   `estimate_transfer_overhead` for the conventions).
/// - **Transfer overhead**: the input transfer, approval, and output transfer gas that is NOT
///   captured by `get_amount_out`, computed via `estimate_transfer_overhead`.
/// - **Router overhead**: the user input transfer and — when `ROUTER_FEES_ACTIVE` — the extra
///   output transfer from the fee path. For non-split swaps, callback protocols already include a
///   regular `transferFrom` in their pool gas, so only the Permit2 delta (`USER_PERMIT2_TRANSFER -
///   DEFAULT_TOKEN_TRANSFER_GAS`) is added for Permit2 users; regular transferFrom and vault users
///   pay nothing extra. Non-callback protocols add the full user transfer cost. For split swaps,
///   funds always go through the router first, so the full user input transfer is always added
///   regardless of protocol type.
pub fn estimate_gas_usage(solution: &Solution, strategy: Strategy) -> BigUint {
    let mut total_gas = BigUint::ZERO;
    for swap in solution.swaps() {
        let swap_transfer_overhead = estimate_transfer_overhead(
            &swap.component().protocol_system,
            swap.token_in(),
            swap.token_out(),
            &strategy,
        );
        total_gas += swap_transfer_overhead + swap.estimated_gas();
    }

    // Add user transfer overhead
    if strategy != Strategy::Split {
        if let Some(first_swap) = solution.swaps().first() {
            let protocol: &str = &first_swap.component().protocol_system;
            // if the solution is not a split swap, the protocol gas usage for callback protocols
            // already includes a regular transfer gas usage:
            // - for the permit2 case we need to deduct the transfer usage already included
            // - for the regular transfer from and vault, do nothing, everything is already
            //   accounted
            if PROTOCOLS_CALLBACK.contains(&protocol) {
                if *solution.user_transfer_type() == UserTransferType::TransferFromPermit2 {
                    total_gas += BigUint::from(USER_PERMIT2_TRANSFER - DEFAULT_TOKEN_TRANSFER_GAS);
                }
            } else {
                total_gas += BigUint::from(match *solution.user_transfer_type() {
                    UserTransferType::TransferFromPermit2 => USER_PERMIT2_TRANSFER,
                    _ => DEFAULT_TOKEN_TRANSFER_GAS, /* TransferFrom and UseVaultsFunds have
                                                      * similar overheads */
                });
            }
        }
    } else {
        // for split swaps the funds always go through the router first
        total_gas += BigUint::from(match *solution.user_transfer_type() {
            UserTransferType::TransferFromPermit2 => USER_PERMIT2_TRANSFER,
            _ => DEFAULT_TOKEN_TRANSFER_GAS, /* TransferFrom and UseVaultsFunds have
                                              * similar overheads */
        });
    }

    // Add fees overhead: when fees are active and the last swap's protocol does not
    // already route output through the router, the fee path adds an extra transfer.
    if let Some(last_swap) = solution.swaps().last() {
        let protocol: &str = &last_swap.component().protocol_system;
        let final_swap_output_to_router = (ROUTER_FEES_ACTIVE || strategy == Strategy::Split) &&
            !PROTOCOLS_OUTPUT_TO_ROUTER.contains(&protocol);
        if final_swap_output_to_router {
            total_gas += transfer_token_gas(last_swap.token_out());
        }
    }

    total_gas
}

fn transfer_token_gas(token: &tycho_common::models::token::Token) -> BigUint {
    let measured = token.gas_usage();
    if measured == BigUint::ZERO {
        BigUint::from(DEFAULT_TOKEN_TRANSFER_GAS)
    } else {
        measured
    }
}

/// Gas overhead for token transfers NOT captured by `get_amount_out`.
///
/// `get_amount_out` includes pool computation gas plus any transfers the pool performs during
/// `swap()`. The input transfer is included in `get_amount_out` for:
///
/// - **Callback protocols**: the user pays the pool directly inside the callback.
/// - **ProtocolWillDebit** (`PROTOCOLS_NEEDING_APPROVAL`): the vault pulls from the router via
///   `transferFrom` inside `swap()`.
/// - **Optimizable-transfer-in protocols** (`PROTOCOLS_OPTIMIZABLE_TRANSFER_IN`): the
///   router-to-pool hop is skipped so there is no extra transfer to account for.
/// - **Split strategy**: funds always flow through the router first; the caller accounts for the
///   user input transfer separately.
///
/// For all other protocols (e.g. UniswapV2), the Dispatcher transfers tokens to the pool
/// *before* calling `executor.swap()`, so the cost is NOT included in `get_amount_out`.
///
/// The output transfer (pool → receiver/router) is always included in `get_amount_out`.
///
/// This function adds what's missing:
///
/// - **Input**: for non-callback, non-optimizable, non-split protocols, the `transfer` that happens
///   before `executor.swap()`.
/// - **Approval**: the router's `approve(vault)` for ProtocolWillDebit protocols (Balancer V2,
///   Curve, etc.), regardless of other exclusions.
/// - **Output**: the extra `_transferOut(router, receiver)` for protocols with `outputToRouter =
///   true`, or any protocol when the strategy is Split (output always routes through the router).
fn estimate_transfer_overhead(
    protocol_system: &str,
    token_in: &tycho_common::models::token::Token,
    token_out: &tycho_common::models::token::Token,
    strategy: &Strategy,
) -> BigUint {
    let mut overhead = BigUint::ZERO;

    // Input transfer: only needed when it happens outside swap().
    // - Callback protocols handle it inside the callback (part of swap gas).
    // - Protocols that can have an optimizable transfer in should not be included here either
    //   because the extra transfer is skipped but only if the strategy is not Split
    if !PROTOCOLS_CALLBACK.contains(&protocol_system) &&
        (!PROTOCOLS_OPTIMIZABLE_TRANSFER_IN.contains(&protocol_system) ||
            *strategy == Strategy::Split)
    {
        overhead += transfer_token_gas(token_in);
    }

    if PROTOCOLS_NEEDING_APPROVAL.contains(&protocol_system) {
        overhead += BigUint::from(TOKEN_APPROVAL_GAS);
    }

    // Output transfer: router -> receiver/next pool (only when outputToRouter).
    if PROTOCOLS_OUTPUT_TO_ROUTER.contains(&protocol_system) {
        overhead += transfer_token_gas(token_out);
    }

    overhead
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_common::{models::protocol::ProtocolComponent, Bytes};

    use super::*;
    use crate::encoding::models::{default_token, Solution, Strategy, Swap, UserTransferType};

    fn make_swap(protocol: &str) -> Swap {
        Swap::new(
            ProtocolComponent { protocol_system: protocol.to_string(), ..Default::default() },
            default_token(Bytes::from(vec![0x01u8; 20])),
            default_token(Bytes::from(vec![0x02u8; 20])),
            BigUint::from(100_000u64), // pool gas
        )
    }

    fn make_solution(swaps: Vec<Swap>) -> Solution {
        Solution::new(
            Bytes::from(vec![0x00u8; 20]),
            Bytes::from(vec![0x00u8; 20]),
            Bytes::from(vec![0x01u8; 20]),
            Bytes::from(vec![0x02u8; 20]),
            BigUint::from(1u64),
            BigUint::from(0u64),
            swaps,
        )
    }

    #[test]
    fn test_single_optimizable_transfer_in() {
        // uniswap_v2 is in PROTOCOLS_OPTIMIZABLE_TRANSFER_IN: the router-to-pool input transfer is
        // skipped (tokens are sent directly). Only the user transfer and the fee path transfer
        // remain.
        let solution = make_solution(vec![make_swap("uniswap_v2")]);
        let gas = estimate_gas_usage(&solution, Strategy::Single);

        // user transfer (TransferFrom)         40_000  ← DEFAULT_TOKEN_TRANSFER_GAS
        // input transfer                            0  ← optimizable, skip router→pool hop
        // pool gas                            100_000
        // fee output transfer                  60_000  ← not in OUTPUT_TO_ROUTER, fee path adds it
        assert_eq!(gas, BigUint::from(200_000u64));
    }

    #[test]
    fn test_single_callback_protocol() {
        // uniswap_v3 is a callback protocol: the pool pulls funds inside the callback, so that
        // cost is already in pool gas. The only extra is the fee path's output transfer.
        let solution = make_solution(vec![make_swap("uniswap_v3")]);
        let gas = estimate_gas_usage(&solution, Strategy::Single);

        // user transfer                             0  ← callback + TransferFrom, nothing extra
        // input transfer                            0  ← callback, in pool gas
        // pool gas                            100_000
        // fee output transfer                  60_000  ← TOKEN_GAS, fee path adds _transferOut
        assert_eq!(gas, BigUint::from(160_000u64));
    }

    #[test]
    fn test_single_permit2_with_approval() {
        // rfq:bebop is not callback, not optimizable, and needs an approval (ProtocolWillDebit).
        // With Permit2 the user transfer costs 80k instead of 40k.
        let solution = make_solution(vec![make_swap("rfq:bebop")])
            .with_user_transfer_type(UserTransferType::TransferFromPermit2);
        let gas = estimate_gas_usage(&solution, Strategy::Single);

        // user permit2 transfer                80_000  ← USER_PERMIT2_TRANSFER
        // approval (ProtocolWillDebit)         25_000  ← TOKEN_APPROVAL_GAS
        // input transfer (router → pool)       60_000  ← TOKEN_GAS
        // pool gas                            100_000
        // fee output transfer                  60_000  ← TOKEN_GAS
        assert_eq!(gas, BigUint::from(325_000u64));
    }

    #[test]
    fn test_sequential_two_hops() {
        // Sequential uniswap_v2 → uniswap_v3: uniswap_v2 is in PROTOCOLS_OPTIMIZABLE_TRANSFER_IN
        // so its input transfer is skipped; uniswap_v3 is callback so its input is also
        // skipped. User transfer once (first hop, non-callback). Fee transfer once (last
        // hop, uniswap_v3).
        let solution = make_solution(vec![make_swap("uniswap_v2"), make_swap("uniswap_v3")]);
        let gas = estimate_gas_usage(&solution, Strategy::Sequential);

        // user transfer (first hop, TransferFrom) 40_000  ← DEFAULT_TOKEN_TRANSFER_GAS
        // hop1 input transfer                       0  ← uniswap_v2 is optimizable
        // hop1 pool gas                        100_000
        // hop2 input transfer                       0  ← callback
        // hop2 pool gas                        100_000
        // fee output transfer (last hop)        60_000  ← not in OUTPUT_TO_ROUTER
        assert_eq!(gas, BigUint::from(300_000u64));
    }

    #[test]
    fn test_sequential_two_hops_output_to_router() {
        // Sequential curve → uniswap_v2:
        // vm:curve outputs to router so a new transfer needs to be accounted for at the end of the
        // first hop
        let solution = make_solution(vec![make_swap("vm:curve"), make_swap("uniswap_v2")]);
        let gas = estimate_gas_usage(&solution, Strategy::Sequential);

        // user transfer (first hop, TransferFrom) 40_000  ← DEFAULT_TOKEN_TRANSFER_GAS
        // hop1 pool gas                        100_000
        // hop1 input transfer (router → pool)   60_000  ← vm:curve is not optimizable/callback
        // hop1 approval (ProtocolWillDebit)     25_000  ← TOKEN_APPROVAL_GAS
        // hop1 output to router                 60_000  ← vm:curve in OUTPUT_TO_ROUTER
        // hop2 pool gas                        100_000
        // hop2 input transfer                       0  ← uniswap_v2 is optimizable
        // fee output transfer (last hop)        60_000  ← uniswap_v2 not in OUTPUT_TO_ROUTER
        assert_eq!(gas, BigUint::from(445_000u64));
    }

    #[test]
    fn test_split_two_legs() {
        // Split swap with two equal legs. User transfer once. Extra router transfer at the end
        // because it is a split swap
        let solution = make_solution(vec![
            make_swap("uniswap_v2").with_split(0.5),
            make_swap("uniswap_v3").with_split(0.5),
        ]);
        let gas = estimate_gas_usage(&solution, Strategy::Split);

        // user transfer (TransferFrom)          40_000  ← DEFAULT_TOKEN_TRANSFER_GAS
        // transfer for leg1 (not optimized)     60_000  ← TOKEN_GAS
        // leg1 pool gas                        100_000
        // leg2 pool gas                        100_000
        // extra output transfer (→ router)       60_000  ← TOKEN_GAS
        assert_eq!(gas, BigUint::from(360_000u64));
    }
}
