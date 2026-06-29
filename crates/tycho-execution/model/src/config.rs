//! Global `const` configuration variables that mostly control the tradeoff
//! between simulation thoroughness, i.e. "what's simulated",
//! and the time required to run the simulation.
//!
//! Disabling functionality here should lead to the disabled code
//! becoming unreachable. Since unreachable code is removed by the compiler,
//! disabled functionality should not incur any runtime cost.

/// Number of sequential and split swaps to simulate.
/// assumption: reverts if `swap_count = 0`. no need to simulate that.
/// each increment exponentially increases the time required
/// to run the simulation.
pub const SWAP_COUNT: i64 = 1;
/// compile time assertion that ensures range of swap count
const _: () = assert!(0 < SWAP_COUNT && SWAP_COUNT < 3);

/// Setting this to the number of cores in your machine plus a couple
/// usually gives the best performance
/// measured in `count_simulated_per_second`.
/// if you're doing other work on the machine,
/// reduce the number to keep a couple of cores usable by
/// the web browser, etc.
pub const WORKER_THREAD_COUNT: usize = 10;

/// Allows you to toggle whether values other than `0`
/// are simulated for the fee bps variables like `client_fee_bps`.
/// setting this to `true` makes the simulation more thorough
/// at the cost of needing longer to run.
pub const ENABLE_NONZERO_FEE_BPS: bool = false;

/// Whether to simulate positive slippage scenarios
/// (actual_amount_out > expected_amount_out).
/// When enabled, the fee calculator splits surplus between
/// the router and the client.
pub const ENABLE_POSITIVE_SLIPPAGE: bool = false;

/// Setting this to `true` introduces a fault,
/// a transfer of 1000 WETH to `msg.sender` inside `singleSwap`,
/// and should result in the output of suspicious [Outcome](crate::Outcome)s.
/// useful as a sanity check or when debugging the code that finds
/// [Outcome](crate::Outcome)s suspicious and serializes them.
pub const INTRODUCE_FAULT: bool = false;

/// Whether to simulate `singleSwap` (unless [ENABLE_BASE_SWAP_FUNCTIONS] is `false`),
/// `singleSwapUsingVault` (unless [ENABLE_VAULT_SWAP_FUNCTIONS] is `false`),
/// and `singleSwapPermit2` (unless [ENABLE_PERMIT2_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_SINGLE_SWAP_FUNCTIONS: bool = true;
/// Whether to simulate `sequentialSwap` (unless [ENABLE_BASE_SWAP_FUNCTIONS] is `false`),
/// `sequentialSwapUsingVault` (unless [ENABLE_VAULT_SWAP_FUNCTIONS] is `false`),
/// and `sequentialSwapPermit2` (unless [ENABLE_PERMIT2_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_SEQUENTIAL_SWAP_FUNCTIONS: bool = true;
/// Whether to simulate `splitSwap` (unless [ENABLE_BASE_SWAP_FUNCTIONS] is `false`),
/// `splitSwapUsingVault` (unless [ENABLE_VAULT_SWAP_FUNCTIONS] is `false`),
/// and `splitSwapPermit2` (unless [ENABLE_PERMIT2_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_SPLIT_SWAP_FUNCTIONS: bool = true;

/// compile time assertion that ensures one set of functions is enabled
const _: () = assert!(
    ENABLE_SINGLE_SWAP_FUNCTIONS || ENABLE_SEQUENTIAL_SWAP_FUNCTIONS || ENABLE_SPLIT_SWAP_FUNCTIONS
);

/// Whether to simulate `singleSwap` (unless [ENABLE_SINGLE_SWAP_FUNCTIONS] is `false`),
/// `sequentialSwap` (unless [ENABLE_SEQUENTIAL_SWAP_FUNCTIONS] is `false`),
/// and `splitSwap` (unless [ENABLE_SPLIT_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_BASE_SWAP_FUNCTIONS: bool = true;
/// Whether to simulate `singleSwapUsingVault` (unless [ENABLE_SINGLE_SWAP_FUNCTIONS] is `false`),
/// `sequentialSwapUsingVault` (unless [ENABLE_SEQUENTIAL_SWAP_FUNCTIONS] is `false`),
/// and `splitSwapUsingVault` (unless [ENABLE_SPLIT_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_VAULT_SWAP_FUNCTIONS: bool = true;
/// Whether to simulate `singleSwapPermit2` (unless [ENABLE_SINGLE_SWAP_FUNCTIONS] is `false`),
/// `sequentialSwapPermit2` (unless [ENABLE_SEQUENTIAL_SWAP_FUNCTIONS] is `false`),
/// and `splitSwapPermit2` (unless [ENABLE_SPLIT_SWAP_FUNCTIONS] is `false`).
pub const ENABLE_PERMIT2_SWAP_FUNCTIONS: bool = true;

/// compile time assertion that ensures one set of functions is enabled
const _: () = assert!(
    ENABLE_BASE_SWAP_FUNCTIONS || ENABLE_VAULT_SWAP_FUNCTIONS || ENABLE_PERMIT2_SWAP_FUNCTIONS
);
