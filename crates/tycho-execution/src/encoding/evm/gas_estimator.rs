use num_bigint::BigUint;

use crate::encoding::models::{Solution, Strategy, UserTransferType};

/// Default gas cost for an ERC-20 `transferFrom` or `transfer`. Used as fallback when the token
/// has no measured gas cost.
pub const DEFAULT_TOKEN_TRANSFER_GAS: u64 = 40_000;

/// Gas cost for an ERC-20 `approve`.
pub const TOKEN_APPROVAL_GAS: u64 = 25_000;

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

/// ProtocolWillDebit: the router must `approve(protocol)` before swapping.
/// The protocol's `transferFrom` is inside `swap()` and already in the gas computation of
/// `get_amount_out`, but the approval is not.
pub const PROTOCOLS_NEEDING_APPROVAL: &[&str] =
    &["vm:balancer_v2", "vm:curve", "rfq:bebop", "rfq:hashflow", "rfq:liquorice", "erc4626"];

/// `outputToRouter = true`: the pool sends output to the router, which then does an extra
/// `_transferOut` to the receiver.
pub const PROTOCOLS_OUTPUT_TO_ROUTER: &[&str] = &["vm:curve", "rocketpool", "fluid_v1", "weth"];

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
/// - **Router overhead**: the user input transfer (transferFrom / Permit2 / vault), and — when
///   `ROUTER_FEES_ACTIVE` — the extra output transfer added by the fee path.
pub fn estimate_gas_usage(solution: &Solution, strategy: Strategy) -> BigUint {
    let mut total_gas = BigUint::ZERO;
    for swap in solution.swaps() {
        // TODO: sequential swap with optimized transfers
        let swap_transfer_overhead = estimate_transfer_overhead(
            &swap.component().protocol_system,
            swap.token_in(),
            swap.token_out(),
            &strategy,
        );
        total_gas += swap_transfer_overhead + swap.estimated_gas();
    }

    // Add user transfer overhead
    total_gas += BigUint::from(match *solution.user_transfer_type() {
        UserTransferType::TransferFromPermit2 => 80_000u64,
        _ => 40_000u64, // TransferFrom and UseVaultsFunds have similar overheads
    });

    // Add fees overhead: when fees are active and the last swap's protocol does not
    // already route output through the router, the fee path adds an extra transfer.
    if ROUTER_FEES_ACTIVE {
        if let Some(last_swap) = solution.swaps().last() {
            let protocol: &str = &last_swap.component().protocol_system;
            if !PROTOCOLS_OUTPUT_TO_ROUTER.contains(&protocol) && strategy != Strategy::Split {
                total_gas += transfer_token_gas(last_swap.token_out());
            }
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
/// `swap()`. The convention is:
///
/// - Input transfer is in `get_amount_out` when the pool handles it inside `swap()`: callback
///   protocols (user pays pool directly in the callback) and ProtocolWillDebit (vault pulls from
///   router via `transferFrom`). For other protocols (e.g. UniV2) the Dispatcher transfers tokens
///   to the pool before calling `executor.swap()`, so it's NOT included in `get_amount_out`.
/// - Output transfer (pool -> receiver/router) is always included in `get_amount_out`.
///
/// This function adds what's missing:
///
/// - **Input**: for non-callback protocols, the `transferFrom` that happens before
///   `executor.swap()`.
/// - **Approval**: the router's `approve(vault)` for ProtocolWillDebit protocols (Balancer V2,
///   Curve, etc.).
/// - **Output**: the extra `_transferOut(router, receiver)` for protocols with `outputToRouter =
///   true`.
fn estimate_transfer_overhead(
    protocol_system: &str,
    token_in: &tycho_common::models::token::Token,
    token_out: &tycho_common::models::token::Token,
    strategy: &Strategy,
) -> BigUint {
    let mut overhead = BigUint::ZERO;

    // Input transfer: only needed when it happens outside swap().
    // Callback protocols handle it inside the callback (part of swap gas).
    if !PROTOCOLS_CALLBACK.contains(&protocol_system) {
        overhead += transfer_token_gas(token_in);
    }

    if PROTOCOLS_NEEDING_APPROVAL.contains(&protocol_system) {
        overhead += BigUint::from(TOKEN_APPROVAL_GAS);
    }

    // Output transfer: router -> receiver/next pool (only when outputToRouter or strategy is
    // Split).
    if PROTOCOLS_OUTPUT_TO_ROUTER.contains(&protocol_system) || *strategy == Strategy::Split {
        overhead += transfer_token_gas(token_out);
    }

    overhead
}
