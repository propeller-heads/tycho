//! PancakeSwap Infinity Bin pools (LBAMM)
//!
//! Liquidity sits in discrete bins. Bin `id` holds a constant-sum curve at a fixed price
//! `(1 + bin_step / 10_000) ^ (id - 2^23)`, so a swap walks bins one at a time, draining the
//! output-side reserve of each, and the quote is exact per bin with no square roots.
//!
//! State comes from the `pancakeswap_infinity_bin` substreams package:
//! - static: `bin_step` (u16 BE), `key_lp_fee` (LP fee in pips)
//! - dynamic: `active_id`, `fee` (the LP fee again, preferred over the static one so a governance
//!   update lands), `protocol_fees/zero2one`, `protocol_fees/one2zero`, `bins/{id}` (the raw
//!   32-byte `reserveOfBin` word, y in the high 128 bits, x in the low)
//!
//! That package indexes only pools with a static LP fee and no swap hook, so nothing here models a
//! hook or a fee that moves mid-swap.
//!
//! All math is ported from `pancakeswap/infinity-core` at commit `7c04695f`, the one the
//! deployed BinPoolManager was built from; every `path:line` in this module refers to it. The
//! port has to be bit-exact, rounding included: a quote that rounds one wei the other way than
//! the pool does is a failed execution, not a rounding difference.
mod attributes;
mod decoder;
mod math;
pub mod state;
