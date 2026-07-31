//! <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/TychoRouter.sol>
#![allow(clippy::too_many_arguments)]
use crate::{
    address::Address,
    error::Error,
    log::{Event, Log},
    math::checked_subtract,
    model::{
        dispatcher::{_call_get_effective_router_fee_on_output, _call_swap_on_executor},
        executors::Executor,
        fee_calculator::calculate_fee,
        transfer_manager::{_transfer_out, _tstore_transfer_from_info},
        vault::Vault,
    },
    params::{ParamKey, Params},
    state::State,
};

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TychoRouter.sol#L184>
pub fn split_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    n_tokens: i64,
    receiver: Address,
) -> Result<(), Error> {
    _update_native_delta_accounting(params, state, vault, log, amount_in)?;
    _tstore_transfer_from_info(state, token_in, amount_in, false, false);

    _split_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        n_tokens,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/9b0512c9580617224c7a0d7de781674a2cdc6b62/foundry/src/TychoRouter.sol#L227>
pub fn split_swap_using_vault(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    n_tokens: i64,
    receiver: Address,
) -> Result<(), Error> {
    _tstore_transfer_from_info(state, token_in, amount_in, false, true);

    _split_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        n_tokens,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/c5b5c4209bc7cb560e9c662d264d499f972527f1/foundry/src/TychoRouter.sol#L293>
pub fn split_swap_permit2(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    n_tokens: i64,
    receiver: Address,
) -> Result<(), Error> {
    if token_in != Address::NativeETH {
        state.permit2_permit(state.msg_sender());
    }
    _tstore_transfer_from_info(state, token_in, amount_in, true, false);

    _split_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        n_tokens,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/9b0512c9580617224c7a0d7de781674a2cdc6b62/foundry/src/TychoRouter.sol#L324>
pub fn sequential_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    _update_native_delta_accounting(params, state, vault, log, amount_in)?;
    _tstore_transfer_from_info(state, token_in, amount_in, false, false);

    _sequential_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L366>
pub fn sequential_swap_using_vault(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    _tstore_transfer_from_info(state, token_in, amount_in, false, true);

    _sequential_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/c5b5c4209bc7cb560e9c662d264d499f972527f1/foundry/src/TychoRouter.sol#L453>
pub fn sequential_swap_permit2(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    if token_in != Address::NativeETH {
        state.permit2_permit(state.msg_sender());
    }
    _tstore_transfer_from_info(state, token_in, amount_in, true, false);

    _sequential_swap_checked(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L457>
pub fn single_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    _update_native_delta_accounting(params, state, vault, log, amount_in)?;
    _tstore_transfer_from_info(state, token_in, amount_in, false, false);

    if crate::config::INTRODUCE_FAULT {
        state.erc20_safe_transfer(Address::WETH, Address::Router, Address::Sender, 1000)?;
    }

    _single_swap(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L499>
pub fn single_swap_using_vault(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    _tstore_transfer_from_info(state, token_in, amount_in, false, true);

    _single_swap(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L499>
pub fn single_swap_permit2(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    if token_in != Address::NativeETH {
        state.permit2_permit(state.msg_sender());
    }
    _tstore_transfer_from_info(state, token_in, amount_in, true, false);

    _single_swap(
        params,
        state,
        vault,
        log,
        amount_in,
        token_in,
        token_out,
        min_amount_out,
        receiver,
    )
}

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TychoRouter.sol#L603>
fn _split_swap_checked(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    n_tokens: i64,
    receiver: Address,
) -> Result<(), Error> {
    if amount_in == 0 {
        return Err(Error::revert("_split_swap_checked: amount_in == 0"));
    }
    if receiver == Address::Zero {
        return Err(Error::revert("_split_swap_checked: receiver == address(0)"));
    }
    if min_amount_out == 0 {
        return Err(Error::revert("_split_swap_checked: min_amount_out == 0"));
    }

    let router_fee_on_output_bps = _call_get_effective_router_fee_on_output(params, state)?;

    let client_fee_bps = if crate::config::ENABLE_NONZERO_FEE_BPS {
        params.request(
            ParamKey::String("client_fee_bps"),
            [0, crate::model::fee_calculator::MAX_FEE_BPS],
        )?
    } else {
        0
    };

    let final_receiver =
        determine_final_receiver(receiver, client_fee_bps, router_fee_on_output_bps)?;

    let amount_out_before_fees = _split_swap(
        params,
        state,
        vault,
        log,
        amount_in,
        n_tokens,
        final_receiver,
        token_in == token_out,
    )?;

    let amount_out = if client_fee_bps == 0 && router_fee_on_output_bps == 0 {
        amount_out_before_fees
    } else {
        _take_fees(params, vault, log, token_out, amount_out_before_fees, client_fee_bps)?
    };

    let amount_out = _maybe_add_client_contribution(
        params,
        state,
        vault,
        log,
        amount_out,
        min_amount_out,
        token_out,
        receiver,
    )?;

    _settle_output(state, vault, log, amount_out, amount_in, token_in, token_out, receiver)?;
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L651>
fn _single_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    if amount_in == 0 {
        return Err(Error::revert("_single_swap: amount_in == 0"));
    }
    if receiver == Address::Zero {
        return Err(Error::revert("_single_swap: receiver == address(0)"));
    }
    if min_amount_out == 0 {
        return Err(Error::revert("_single_swap: min_amount_out == 0"));
    }

    let router_fee_on_output_bps = _call_get_effective_router_fee_on_output(params, state)?;

    let client_fee_bps = if crate::config::ENABLE_NONZERO_FEE_BPS {
        params.request(
            ParamKey::String("client_fee_bps"),
            [0, crate::model::fee_calculator::MAX_FEE_BPS],
        )?
    } else {
        0
    };

    let final_receiver =
        determine_final_receiver(receiver, client_fee_bps, router_fee_on_output_bps)?;

    let swap_index = 0;
    let executor = params.request(ParamKey::Executor { swap_index }, Executor::VARIANTS)?;

    let amount_out_before_fees = _call_swap_on_executor(
        params,
        state,
        vault,
        log,
        executor,
        amount_in,
        true,
        false,
        final_receiver,
        swap_index,
    )?;

    let amount_out = if client_fee_bps == 0 && router_fee_on_output_bps == 0 {
        amount_out_before_fees
    } else {
        _take_fees(params, vault, log, token_out, amount_out_before_fees, client_fee_bps)?
    };

    let amount_out = _maybe_add_client_contribution(
        params,
        state,
        vault,
        log,
        amount_out,
        min_amount_out,
        token_out,
        receiver,
    )?;

    _settle_output(state, vault, log, amount_out, amount_in, token_in, token_out, receiver)?;
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L717>
fn _sequential_swap_checked(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    min_amount_out: i64,
    receiver: Address,
) -> Result<(), Error> {
    if amount_in == 0 {
        return Err(Error::revert("_sequential_swap_checked: amount_in == 0"));
    }
    if receiver == Address::Zero {
        return Err(Error::revert("_sequential_swap_checked: receiver == address(0)"));
    }
    if min_amount_out == 0 {
        return Err(Error::revert("_sequential_swap_checked: min_amount_out == 0"));
    }

    let router_fee_on_output_bps = _call_get_effective_router_fee_on_output(params, state)?;

    let client_fee_bps = if crate::config::ENABLE_NONZERO_FEE_BPS {
        params.request(
            ParamKey::String("client_fee_bps"),
            [0, crate::model::fee_calculator::MAX_FEE_BPS],
        )?
    } else {
        0
    };

    let final_receiver =
        determine_final_receiver(receiver, client_fee_bps, router_fee_on_output_bps)?;

    let amount_out_before_fees =
        _sequential_swap(params, state, vault, log, amount_in, final_receiver)?;

    let amount_out = if client_fee_bps == 0 && router_fee_on_output_bps == 0 {
        amount_out_before_fees
    } else {
        _take_fees(params, vault, log, token_out, amount_out_before_fees, client_fee_bps)?
    };

    let amount_out = _maybe_add_client_contribution(
        params,
        state,
        vault,
        log,
        amount_out,
        min_amount_out,
        token_out,
        receiver,
    )?;

    _settle_output(state, vault, log, amount_out, amount_in, token_in, token_out, receiver)?;
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L773>
fn _settle_output(
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    mut amount_out: i64,
    amount_in: i64,
    token_in: Address,
    token_out: Address,
    receiver: Address,
) -> Result<i64, Error> {
    let output_delta = vault._get_delta(token_out);
    if output_delta > 0 {
        vault._update_delta_accounting(token_out, -amount_out);
        log.append(Event::UpdateDeltaAccounting {
            token: token_out,
            delta_change: -amount_out,
            nonzero_delta_count_after: vault._get_nonzero_delta_count(),
            context_hint: "_settle_output: output_delta > 0",
        });
        if receiver == Address::Router {
            vault._credit_vault(state.msg_sender(), token_out, amount_out)?;
            log.append(Event::CreditVault {
                owner: state.msg_sender(),
                token: token_out,
                amount: amount_out,
                context_hint: "_settle_output: output_delta > 0 && receiver == Address::Router",
            });
        } else {
            amount_out = _transfer_out(state, token_out, receiver, amount_out)?;
            log.append(Event::TransferOut {
                token: token_out,
                receiver,
                amount: amount_out,
                context_hint: "_settle_output: output_delta > 0 && receiver != Address::Router",
            });
        }
    }

    vault._finalize_balances(state, state.msg_sender(), token_in, amount_in)?;

    Ok(amount_out)
}

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TychoRouter.sol#L862>
fn _split_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    n_tokens: i64,
    receiver: Address,
    is_cyclical: bool,
) -> Result<i64, Error> {
    // assumption: reverts if `swap_count = 0`
    let swap_count =
        u8::try_from(params.request("swap_count", [crate::config::SWAP_COUNT])?).unwrap();

    let mut remaining_amounts = vec![0i64; usize::try_from(n_tokens).unwrap()];
    let mut amounts = vec![0i64; usize::try_from(n_tokens).unwrap()];
    let mut cyclic_swap_amount_out: i64 = 0;
    amounts[0] = amount_in;
    remaining_amounts[0] = amount_in;

    for swap_index in 0..swap_count {
        // inlined code from https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/lib/LibSwap.sol#L32
        let token_in_index =
            params.request(ParamKey::SwapData { swap_index, start: 0, end: 1 }, 0..n_tokens)?;
        let token_out_index =
            params.request(ParamKey::SwapData { swap_index, start: 1, end: 2 }, 0..n_tokens)?;
        let largest_uint24 = 2i64.pow(24) - 1;
        let split = params.request(
            ParamKey::SwapData { swap_index, start: 2, end: 5 },
            [0, 1, 10000, largest_uint24],
        )?;
        let executor = params.request(ParamKey::Executor { swap_index }, Executor::VARIANTS)?;

        // TODO check overflow for multiplication
        let current_amount_in = if split > 0 {
            amounts[usize::try_from(token_in_index).unwrap()] * split / 0xffffff
        } else {
            remaining_amounts[usize::try_from(token_in_index).unwrap()]
        };

        let mut swap_receiver = Address::Router;
        if token_out_index == checked_subtract(n_tokens, 1)? && !is_cyclical ||
            is_cyclical && token_out_index == 0
        {
            swap_receiver = receiver;
        }

        let current_amount_out = _call_swap_on_executor(
            params,
            state,
            vault,
            log,
            executor,
            current_amount_in,
            token_in_index == 0,
            true,
            swap_receiver,
            swap_index,
        )?;

        if token_out_index == 0 {
            cyclic_swap_amount_out += current_amount_out;
        } else {
            amounts[usize::try_from(token_out_index).unwrap()] += current_amount_out;
        }
        remaining_amounts[usize::try_from(token_out_index).unwrap()] += current_amount_out;
        remaining_amounts[usize::try_from(token_in_index).unwrap()] -= current_amount_out;
    }
    Ok(if is_cyclical {
        cyclic_swap_amount_out
    } else {
        amounts[usize::try_from(checked_subtract(n_tokens, 1)?).unwrap()]
    })
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L902>
fn _sequential_swap(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
    final_receiver: Address,
) -> Result<i64, Error> {
    let mut calculated_amount = amount_in;

    // assumption: reverts if `swap_count = 0`
    let swap_count =
        u8::try_from(params.request("swap_count", [crate::config::SWAP_COUNT])?).unwrap();

    for swap_index in 0..swap_count {
        let is_last_swap = swap_index == swap_count - 1;

        let receiver = if is_last_swap {
            final_receiver
        } else {
            let next_swap_index = swap_index + 1;
            let next_executor = params
                .request(ParamKey::Executor { swap_index: next_swap_index }, Executor::VARIANTS)?;
            next_executor.funds_expected_address(params, state, next_swap_index)?
        };

        let executor = params.request(ParamKey::Executor { swap_index }, Executor::VARIANTS)?;

        calculated_amount = _call_swap_on_executor(
            params,
            state,
            vault,
            log,
            executor,
            calculated_amount,
            swap_index == 0,
            false,
            receiver,
            swap_index,
        )?;
    }

    Ok(calculated_amount)
}

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TychoRouter.sol#L1058>
fn _take_fees(
    params: &Params,
    vault: &mut Vault,
    log: &mut impl Log,
    token: Address,
    amount_in: i64,
    client_fee_bps: i64,
) -> Result<i64, Error> {
    let (amount_out, fees) = calculate_fee(params, amount_in, client_fee_bps)?;

    for fee in fees {
        if fee.fee_amount > 0 {
            vault._update_delta_accounting(token, -fee.fee_amount);
            log.append(Event::UpdateDeltaAccounting {
                token,
                delta_change: -fee.fee_amount,
                nonzero_delta_count_after: vault._get_nonzero_delta_count(),
                context_hint: "_take_fees: fee_amount > 0",
            });

            vault._credit_vault_for_fees(fee.recipient, token, fee.fee_amount)?;
            log.append(Event::CreditVault {
                token,
                owner: fee.recipient,
                amount: fee.fee_amount,
                context_hint: "_take_fees: fee_amount > 0",
            });
        }
    }

    Ok(amount_out)
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L1063>
fn _update_native_delta_accounting(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_in: i64,
) -> Result<(), Error> {
    // assumption: reverts unless `msg_value > 0 -> msg_value == amount_in`
    let msg_value = params.request("msg_value", vec![0, amount_in])?;
    if msg_value > 0 {
        if msg_value != amount_in {
            return Err(Error::revert("update_native_delta_accounting: msg_value != amount_in"));
        }
        state.eth_send_value(Address::Sender, Address::Router, msg_value)?;
        vault._update_delta_accounting(Address::NativeETH, msg_value);
        log.append(Event::UpdateDeltaAccounting {
            token: Address::NativeETH,
            delta_change: msg_value,
            nonzero_delta_count_after: vault._get_nonzero_delta_count(),
            context_hint: "_update_native_delta_accounting: msg_value > 0",
        });
    }
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/9b0512c9580617224c7a0d7de781674a2cdc6b62/foundry/src/TychoRouter.sol#L1093>
fn _maybe_add_client_contribution(
    params: &Params,
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    amount_out: i64,
    min_amount_out: i64,
    token_out: Address,
    receiver: Address,
) -> Result<i64, Error> {
    if amount_out < min_amount_out {
        let required_contribution = checked_subtract(min_amount_out, amount_out)?;
        // assumption: reverts if `required_contribution > max_client_contribution`
        let max_client_contribution = params.request(
            "max_client_contribution",
            [required_contribution, required_contribution + 1],
        )?;
        if required_contribution > max_client_contribution {
            return Err(Error::revert(
                "_maybe_add_client_contribution: required_contribution > max_client_contribution",
            ));
        }
        vault._debit_vault(Address::ClientFeeReceiver, token_out, required_contribution)?;
        log.append(Event::DebitVault {
            token: token_out,
            owner: Address::ClientFeeReceiver,
            amount: required_contribution,
            context_hint: "_maybe_add_client_contribution: amount_out < min_amount_out",
        });
        let output_delta = vault._get_delta(token_out);
        if output_delta > 0 {
            vault._update_delta_accounting(token_out, required_contribution);
            log.append(Event::UpdateDeltaAccounting {
                token: token_out,
                delta_change: required_contribution,
                nonzero_delta_count_after: vault._get_nonzero_delta_count(),
                context_hint: "_maybe_add_client_contribution: amount_out < min_amount_out && output_delta > 0",
            });
        } else if output_delta == 0 {
            if receiver == Address::Router {
                vault._credit_vault(state.msg_sender(), token_out, required_contribution)?;
                log.append(Event::CreditVault {
                    token: token_out,
                    owner: state.msg_sender(),
                    amount: required_contribution,
                    context_hint: "_maybe_add_client_contribution: amount_out < min_amount_out && output_delta == 0 && receiver == Address::Router",
                });
            } else if token_out == Address::NativeETH {
                state.eth_send_value(Address::Router, receiver, required_contribution)?;
                log.append(Event::EthSendValue {
                    sender: Address::Router,
                    receiver,
                    amount: required_contribution,
                    context_hint: "_maybe_add_client_contribution: amount_out < min_amount_out && output_delta == 0 && receiver == Address::Router && token_out == address(0)",
                });
            } else {
                state.erc20_safe_transfer(
                    token_out,
                    Address::Router,
                    receiver,
                    required_contribution,
                )?;
                log.append(Event::Erc20SafeTransfer {
                    token: token_out,
                    sender: Address::Router,
                    receiver,
                    amount: required_contribution,
                    context_hint: "_maybe_add_client_contribution: amount_out < min_amount_out && output_delta == 0 && receiver == Address::Router && token_out != address(0)",
                });
            }
        } else {
            return Err(Error::revert("_maybe_add_client_contribution: negative output delta"));
        }

        Ok(min_amount_out)
    } else {
        Ok(amount_out)
    }
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TychoRouter.sol#L1125>
fn determine_final_receiver(
    receiver: Address,
    client_fee_bps: i64,
    router_fee_on_output_bps: i64,
) -> Result<Address, Error> {
    Ok(if client_fee_bps == 0 && router_fee_on_output_bps == 0 {
        receiver
    } else {
        Address::Router
    })
}
