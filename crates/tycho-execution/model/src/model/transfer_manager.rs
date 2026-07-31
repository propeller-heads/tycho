//! <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/TransferManager.sol>
use serde::Serialize;

use crate::{
    address::Address,
    error::Error,
    log::{Event, Log},
    math::checked_subtract,
    model::vault::Vault,
    state::State,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum TransferType {
    Transfer,
    TransferNativeInExecutor,
    ProtocolWillDebit,
    None,
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TransferManager.sol#L83>
pub fn _tstore_transfer_from_info(
    state: &mut State,
    token_in: Address,
    amount_in: i64,
    is_permit2: bool,
    use_vault: bool,
) {
    state.tstore("token_in", token_in);
    state.tstore("amount_allowed", if use_vault { 0 } else { amount_in });
    state.tstore("is_permit2", is_permit2);
    state.tstore("sender", state.msg_sender());
    state.tstore("use_vault", use_vault);
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TransferManager.sol#L130>
#[allow(clippy::too_many_arguments)]
pub fn _transfer(
    state: &mut State,
    vault: &mut Vault,
    log: &mut impl Log,
    receiver: Address,
    transfer_type: TransferType,
    token_in: Address,
    amount: i64,
    is_first_swap: bool,
    is_split_swap: bool,
    in_callback: bool,
) -> Result<i64, Error> {
    let needs_transfer_from_user = is_first_swap && !state.tload("use_vault")?;
    match transfer_type {
        TransferType::None => Ok(amount),
        TransferType::TransferNativeInExecutor => {
            vault._update_delta_accounting(token_in, -amount);
            log.append(Event::UpdateDeltaAccounting {
                token: token_in,
                delta_change: -amount,
                nonzero_delta_count_after: vault._get_nonzero_delta_count(),
                context_hint: "_transfer: TransferNativeInExecutor",
            });
            Ok(amount)
        }
        TransferType::ProtocolWillDebit => {
            if needs_transfer_from_user {
                let amount = _transfer_from_user(state, token_in, Address::Router, amount)?;
                _approve_if_needed(state, token_in, receiver, amount)?;
                Ok(amount)
            } else {
                vault._update_delta_accounting(token_in, -amount);
                log.append(Event::UpdateDeltaAccounting {
                    token: token_in,
                    delta_change: -amount,
                    nonzero_delta_count_after: vault._get_nonzero_delta_count(),
                    context_hint: "_transfer: ProtocolWillDebit && !needs_transfer_from_user",
                });
                _approve_if_needed(state, token_in, receiver, amount)?;
                Ok(amount)
            }
        }
        TransferType::Transfer => {
            let can_use_sequential_swap_optimization =
                !is_first_swap && !is_split_swap && !in_callback;

            if can_use_sequential_swap_optimization {
                return Ok(amount);
            }

            if needs_transfer_from_user {
                _transfer_from_user(state, token_in, receiver, amount)
            } else {
                vault._update_delta_accounting(token_in, -amount);
                log.append(Event::UpdateDeltaAccounting {
                    token: token_in,
                    delta_change: -amount,
                    nonzero_delta_count_after: vault._get_nonzero_delta_count(),
                    context_hint: "_transfer: Transfer && !needs_transfer_from_user",
                });
                let amount = _transfer_out(state, token_in, receiver, amount)?;
                log.append(Event::TransferOut {
                    token: token_in,
                    receiver,
                    amount,
                    context_hint: "_transfer: Transfer && !needs_transfer_from_user",
                });
                Ok(amount)
            }
        }
    }
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TransferManager.sol#L231>
pub fn _approve_if_needed(
    state: &mut State,
    token: Address,
    receiver: Address,
    amount: i64,
) -> Result<(), Error> {
    if receiver != Address::Router {
        state.erc20_force_approve(token, Address::Router, receiver, amount)?;
    }
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TransferManager.sol#L225>
pub fn _revoke_unconsumed_approval(
    state: &mut State,
    token: Address,
    spender: Address,
) -> Result<(), Error> {
    if state.erc20_allowance(token, Address::Router, spender)? > 0 {
        state.erc20_force_approve(token, Address::Router, spender, 0)?;
    }
    Ok(())
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TransferManager.sol#L246>
fn _transfer_from_user(
    state: &mut State,
    token: Address,
    receiver: Address,
    amount: i64,
) -> Result<i64, Error> {
    let token_in_storage = state.tload("token_in")?;
    let mut amount_allowed = state.tload("amount_allowed")?;
    let sender = state.tload("sender")?;
    let is_permit2 = state.tload("is_permit2")?;

    if amount > amount_allowed {
        return Err(Error::revert("_transfer_from_user: amount > amount_allowed"));
    }

    if token != token_in_storage {
        return Err(Error::revert("_transfer_from_user: token != token_in_storage"));
    }

    amount_allowed -= amount;

    state.tstore("amount_allowed", amount_allowed);

    let balance_before = _balance_of(state, token, receiver)?;

    if is_permit2 {
        state.permit2_transfer_from(Address::Router, sender, receiver, amount, token)?;
    } else {
        state.erc20_safe_transfer_from(token, Address::Router, sender, receiver, amount)?;
    }

    let balance_after = _balance_of(state, token, receiver)?;
    checked_subtract(balance_after, balance_before)
}

/// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/TransferManager.sol#L291>
pub fn _balance_of(state: &mut State, token: Address, owner: Address) -> Result<i64, Error> {
    if token == Address::NativeETH {
        Ok(state.eth_balance(owner))
    } else {
        state.erc20_balance_of(token, owner)
    }
}

/// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/TransferManager.sol#L231>
pub fn _transfer_out(
    state: &mut State,
    token: Address,
    to: Address,
    amount: i64,
) -> Result<i64, Error> {
    if token == Address::NativeETH {
        state.eth_send_value(Address::Router, to, amount)?;
    } else {
        state.erc20_safe_transfer(token, Address::Router, to, amount)?;
    }
    // assumption: transfer succeeds
    Ok(amount)
}
