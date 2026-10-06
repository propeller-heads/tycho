//! Uniswap V4 hook handlers: analytic models of a pool's hook that let [`UniswapV4State`] price a
//! swap without simulating the hook's own EVM bytecode.
//!
//! [`hook_handler_creator`] keys the registry by `(Chain, hook address)`, since the same address
//! on two chains names two different deployments. A hook with a registered native handler always
//! uses it; an unregistered hook falls back to [`generic_vm_hook_handler`] on Ethereum and
//! Unichain only, and fails to decode everywhere else. [`angstrom`] is the native handler for
//! Angstrom's hook on Ethereum; [`pons_v2`] is the native handler for the Pons V2 MemeHook on
//! Robinhood.
//!
//! [`UniswapV4State`]: crate::evm::protocol::uniswap_v4::state::UniswapV4State

pub mod angstrom;
pub mod generic_vm_hook_handler;
pub mod hook_handler;
pub mod hook_handler_creator;
pub mod models;
pub mod pons_v2;
pub mod utils;
