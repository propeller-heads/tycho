//! <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/Dispatcher.sol>
use crate::{
    Address, Error, State,
    log::{Event, Log},
    math::checked_subtract,
    model::{
        executors::Executor,
        transfer_manager::{
            _balance_of, _revoke_unconsumed_approval, _transfer, _transfer_out, TransferType,
        },
        vault::Vault,
    },
    params::{ParamKey, Params},
};

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/Dispatcher.sol#L89>
#[allow(clippy::too_many_arguments)]
pub fn _call_swap_on_executor(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    executor: Executor,
    amount: i64,
    is_first_swap: bool,
    is_split_swap: bool,
    receiver: Address,
    swap_index: u8,
) -> Result<i64, Error> {
    state.tstore("currently_swapping_executor", executor);
    state.tstore("is_first_swap", is_first_swap);
    state.tstore("is_split_swap", is_split_swap);
    state.tstore("swap_input_amount", amount);

    let transfer_data = executor.get_transfer_data(params, state, swap_index)?;
    log.append(Event::TransferData {
        transfer_data: transfer_data.clone(),
        context_hint: "_call_swap_on_executor",
    });

    if transfer_data.token_in == transfer_data.token_out {
        return Err(Error::revert(
            "_call_swap_on_executor: transfer_data.token_in == transfer_data.token_out",
        ));
    }

    state.tstore("swap_input_token", transfer_data.token_in);

    let measure_at = if transfer_data.output_to_router { Address::Router } else { receiver };

    let balance_before_swap = if measure_at.is_sender_controlled() ||
        transfer_data
            .token_out
            .is_sender_controlled()
    {
        // if the sender controls `token_out`, they can make the `balanceOf`
        // return arbitrary amounts.
        // if the sender controls `measure_at`, they can control the balance.
        params.request(ParamKey::SwapIndexed { swap_index, prefix: "balance_before_swap" }, [0])?
    } else {
        _balance_of(state, transfer_data.token_out, measure_at)?
    };

    let amount = _transfer(
        state,
        vault,
        log,
        transfer_data.receiver,
        transfer_data.transfer_type,
        transfer_data.token_in,
        amount,
        is_first_swap,
        is_split_swap,
        false,
    )?;

    executor.swap(params, state, vault, log, amount, receiver, swap_index)?;

    state.tdelete("currently_swapping_executor");
    state.tstore("is_first_swap", false);
    state.tstore("is_split_swap", false);
    state.tstore("swap_input_amount", 0);
    state.tstore("swap_input_token", Address::Zero);

    if transfer_data.transfer_type == TransferType::ProtocolWillDebit {
        _revoke_unconsumed_approval(state, transfer_data.token_in, transfer_data.receiver)?;
    }

    let balance_after_swap = if measure_at.is_sender_controlled() ||
        transfer_data
            .token_out
            .is_sender_controlled()
    {
        // if the sender controls `token_out`, they can make the `balanceOf`
        // return arbitrary amounts.
        // if the sender controls `measure_at`, they can control the balance.
        params.request(
            ParamKey::SwapIndexed { swap_index, prefix: "balance_after_swap" },
            // simulate a balance increase at no cost to the sender
            [balance_before_swap + 10000],
        )?
    } else {
        _balance_of(state, transfer_data.token_out, measure_at)?
    };

    let mut amount_out = checked_subtract(balance_after_swap, balance_before_swap)?;

    if transfer_data.output_to_router && receiver != Address::Router {
        amount_out = _transfer_out(state, transfer_data.token_out, receiver, amount_out)?;
        log.append(Event::TransferOut {
            token: transfer_data.token_out,
            receiver,
            amount: amount_out,
            context_hint: "_call_swap_on_executor: transfer_data.output_to_router && receiver != Address::Router",
        });
    }

    if receiver == Address::Router {
        vault._update_delta_accounting(transfer_data.token_out, amount_out);
        log.append(Event::UpdateDeltaAccounting {
            token: transfer_data.token_out,
            delta_change: amount_out,
            nonzero_delta_count_after: vault._get_nonzero_delta_count(),
            context_hint: "_call_swap_on_executor: receiver == Address::Router",
        });
    }

    Ok(amount_out)
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/Dispatcher.sol#L198>
pub fn _call_handle_callback_on_executor(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    swap_index: u8,
) -> Result<(), Error> {
    let executor: Executor = state.tload("currently_swapping_executor")?;
    let is_first_swap = state.tload("is_first_swap")?;
    let is_split_swap = state.tload("is_split_swap")?;
    let amount = state.tload("swap_input_amount")?;
    let token_in = state.tload("swap_input_token")?;

    let callback_transfer_data = executor.get_callback_transfer_data(params, state, swap_index)?;

    _transfer(
        state,
        vault,
        log,
        callback_transfer_data.receiver,
        callback_transfer_data.transfer_type,
        token_in,
        amount,
        is_first_swap,
        is_split_swap,
        true,
    )?;

    executor.handle_callback(params, state)?;

    if callback_transfer_data.transfer_type == TransferType::ProtocolWillDebit {
        _revoke_unconsumed_approval(state, token_in, callback_transfer_data.receiver)?;
    }

    state.tdelete("currently_swapping_executor");
    state.tstore("is_first_swap", false);
    state.tstore("is_split_swap", false);

    Ok(())
}
