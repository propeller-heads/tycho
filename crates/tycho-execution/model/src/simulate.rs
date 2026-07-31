use serde::Serialize;

use crate::{
    Address, Error, State,
    log::{Log, VecLog},
    model::{Vault, tycho_router::*},
    params::Params,
};

/// Run the model. The most important function. All [model](crate::model) code is called from this
/// directly or indirectly.
///
/// That [Params] is passed as a read-only borrow implies
/// that [Params] is never mutated during the simulation.
/// Instead, if a new param is [requested](Params::request),
/// [simulate] errors with a [RequestParam](crate::params::RequestParam),
/// which must be resolved by the caller.
/// [simulate] is called by [worker_thread](crate::worker::worker_thread),
/// which resolves [RequestParam](crate::params::RequestParam) by
/// iterating over it and adding the resulting [Params] to its local work queue.
///
/// If you're wondering about the `+ use<>` in the return type:
/// Without it compilation fails and you can find more information here:
/// <https://doc.rust-lang.org/std/keyword.use.html#precise-capturing>
pub fn simulate(params: &Params) -> Result<(State, Vault, impl Log + use<> + Serialize), Error> {
    let mut state = State::default();
    let mut vault = Vault::default();
    let mut log = VecLog::default();

    // assumption: all swap methods revert if `amount_in = 0`.
    // no need to simulate it.
    let amount_in = params.request("amount_in", vec![1, 10000])?;

    // assumption: all swap methods revert if `token_in = 0` or `token_out = 0`.
    // no need to simulate it.
    let token_in = params.request("token_in", Address::VARIANTS_EXCEPT_ZERO)?;
    let token_out = params.request("token_out", Address::VARIANTS_EXCEPT_ZERO)?;

    // assumption: all swap methods revert if `min_amount_out = 0`.
    // no need to simulate it.
    let min_amount_out = params.request("min_amount_out", [1, 10000])?;
    // assumption: all swap methods revert if `receiver = address(0)`.
    // no need to simulate it.
    let receiver = params.request("receiver", Address::VARIANTS_EXCEPT_ZERO)?;

    // compile list of functions to simulate.
    // since these `if` conditions are `const` the compiler should remove them
    // and the resulting code should just be a sequence of pushes.
    let mut functions = vec![];
    if crate::config::ENABLE_SINGLE_SWAP_FUNCTIONS && crate::config::ENABLE_BASE_SWAP_FUNCTIONS {
        functions.push("singleSwap");
    }
    if crate::config::ENABLE_SINGLE_SWAP_FUNCTIONS && crate::config::ENABLE_VAULT_SWAP_FUNCTIONS {
        functions.push("singleSwapUsingVault");
    }
    if crate::config::ENABLE_SINGLE_SWAP_FUNCTIONS && crate::config::ENABLE_PERMIT2_SWAP_FUNCTIONS {
        functions.push("singleSwapPermit2");
    }
    if crate::config::ENABLE_SEQUENTIAL_SWAP_FUNCTIONS && crate::config::ENABLE_BASE_SWAP_FUNCTIONS
    {
        functions.push("sequentialSwap");
    }
    if crate::config::ENABLE_SEQUENTIAL_SWAP_FUNCTIONS && crate::config::ENABLE_VAULT_SWAP_FUNCTIONS
    {
        functions.push("sequentialSwapUsingVault");
    }
    if crate::config::ENABLE_SEQUENTIAL_SWAP_FUNCTIONS &&
        crate::config::ENABLE_PERMIT2_SWAP_FUNCTIONS
    {
        functions.push("sequentialSwapPermit2");
    }
    if crate::config::ENABLE_SPLIT_SWAP_FUNCTIONS && crate::config::ENABLE_BASE_SWAP_FUNCTIONS {
        functions.push("splitSwap");
    }
    if crate::config::ENABLE_SPLIT_SWAP_FUNCTIONS && crate::config::ENABLE_VAULT_SWAP_FUNCTIONS {
        functions.push("splitSwapUsingVault");
    }
    if crate::config::ENABLE_SPLIT_SWAP_FUNCTIONS && crate::config::ENABLE_PERMIT2_SWAP_FUNCTIONS {
        functions.push("splitSwapPermit2");
    }

    match params.request("function", functions)? {
        "singleSwap" => {
            single_swap(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "singleSwapUsingVault" => {
            single_swap_using_vault(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "singleSwapPermit2" => {
            single_swap_permit2(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "sequentialSwap" => {
            sequential_swap(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "sequentialSwapUsingVault" => {
            sequential_swap_using_vault(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "sequentialSwapPermit2" => {
            sequential_swap_permit2(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                receiver,
            )?;
        }
        "splitSwap" => {
            split_swap(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                params.request("n_tokens", [1, 2])?,
                receiver,
            )?;
        }
        "splitSwapUsingVault" => {
            split_swap_using_vault(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                params.request("n_tokens", [1, 2])?,
                receiver,
            )?;
        }
        "splitSwapPermit2" => {
            split_swap_permit2(
                params,
                &mut state,
                &mut vault,
                &mut log,
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                params.request("n_tokens", [1, 2])?,
                receiver,
            )?;
        }
        function => unimplemented!("function {function} is not implemented"),
    }

    Ok((state, vault, log))
}
