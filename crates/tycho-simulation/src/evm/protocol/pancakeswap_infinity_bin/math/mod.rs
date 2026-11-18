//! Bin math, ported from PancakeSwap Infinity core at commit
//! [`7c04695f`](https://github.com/pancakeswap/infinity-core/tree/7c04695f).
//! Every link below pins that commit. Port is bit-exact, rounding included: one wei the wrong way
//! is a failed swap, not a rounding difference.
//!
//! Exact-input quoting only. `getAmountsIn`, `log2` / `getIdFromPrice` and `getCompositionFee`
//! stay unported until a caller needs them.
pub mod bin;
pub mod constants;
pub mod fee;
pub mod uint128x128;
pub mod uint256x256;
