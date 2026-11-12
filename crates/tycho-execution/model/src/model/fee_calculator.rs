//! <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/FeeCalculator.sol>
use crate::{address::Address, error::Error, math::checked_subtract, params::Params};

pub const MAX_BPS: i64 = 100_000_000;
const MAX_BPS_SQUARED: i64 = 10_000_000_000_000_000;

/// <https://github.com/propeller-heads/tycho-execution/blob/9b0512c9580617224c7a0d7de781674a2cdc6b62/foundry/lib/FeeStructs.sol#L9>
pub struct FeeRecipient {
    pub recipient: Address,
    pub fee_amount: i64,
}

struct FeeInfo {
    router_fee_on_output_bps: i64,
    router_fee_on_client_fee_bps: i64,
    router_positive_slippage_bps: i64,
    client_positive_slippage_share_bps: i64,
    has_any_token_pair_fee: bool,
}

fn _get_fee_info(params: &Params) -> Result<FeeInfo, Error> {
    let (router_fee_on_output_bps, router_fee_on_client_fee_bps, has_any_token_pair_fee) =
        if crate::config::ENABLE_NONZERO_FEE_BPS {
            let has_client_custom_fee_on_input =
                params.request("has_client_custom_fee_on_input", vec![true, false])?;

            // A model run has one swap, so it has one token pair.
            let pair_fee_bps: Option<i64> =
                if params.request("has_token_pair_fee", vec![true, false])? {
                    Some(params.request("token_pair_fee_bps", vec![0, MAX_BPS / 2, MAX_BPS])?)
                } else {
                    None
                };
            let has_any_token_pair_fee = pair_fee_bps.is_some() ||
                params.request("has_other_token_pair_fee", vec![true, false])?;

            let router_fee_on_output_bps = match (has_client_custom_fee_on_input, pair_fee_bps) {
                (true, Some(pair_fee_bps)) => params
                    .request("client_fee_bps_on_output", vec![0, MAX_BPS])?
                    .min(pair_fee_bps),
                (true, None) => params.request("client_fee_bps_on_output", vec![0, MAX_BPS])?,
                (false, Some(pair_fee_bps)) => pair_fee_bps,
                (false, None) => params.request("router_fee_on_output_bps", vec![0, MAX_BPS])?,
            };

            let has_client_custom_fee_on_client_fee =
                params.request("has_client_custom_fee_on_client_fee", vec![true, false])?;
            let router_fee_on_client_fee_bps = if has_client_custom_fee_on_client_fee {
                params.request("client_fee_bps_on_client_fee", vec![0, MAX_BPS])?
            } else {
                params.request("router_fee_on_client_fee_bps", vec![0, MAX_BPS])?
            };

            (router_fee_on_output_bps, router_fee_on_client_fee_bps, has_any_token_pair_fee)
        } else {
            (0, 0, false)
        };

    // A model run has one client, so this is that client's router positive slippage bps: its custom
    // value if set, otherwise the global one.
    let router_positive_slippage_bps = if crate::config::ENABLE_POSITIVE_SLIPPAGE {
        params.request("router_positive_slippage_bps", vec![0, MAX_BPS / 2, MAX_BPS])?
    } else {
        0
    };

    let client_positive_slippage_share_bps = if router_positive_slippage_bps > 0 {
        params.request("client_positive_slippage_share_bps", vec![0, MAX_BPS / 2, MAX_BPS])?
    } else {
        0
    };

    Ok(FeeInfo {
        router_fee_on_output_bps,
        router_fee_on_client_fee_bps,
        router_positive_slippage_bps,
        client_positive_slippage_share_bps,
        has_any_token_pair_fee,
    })
}

/// Mirrors `FeeCalculator.calculateFee` in Solidity.
///
/// Returns fee recipients (router + client) with amounts combining both
/// positive slippage surplus and standard fees.
pub fn calculate_fee(
    params: &Params,
    actual_amount_out: i64,
    expected_amount_out: i64,
    client_fee_bps: i64,
    _token_in: Address,
    _token_out: Address,
    _amount_in: i64,
) -> Result<Vec<FeeRecipient>, Error> {
    let fee_info = _get_fee_info(params)?;

    let positive_slippage =
        _calculate_positive_slippage(actual_amount_out, expected_amount_out, &fee_info)?;
    let client_positive_slippage = fee_to_i64(
        positive_slippage as i128 * fee_info.client_positive_slippage_share_bps as i128 /
            MAX_BPS as i128,
    )?;

    let fee_base = actual_amount_out - positive_slippage;

    let (router_fee, client_fee) = _calculate_fee(fee_base, client_fee_bps, &fee_info)?;

    Ok(vec![
        FeeRecipient {
            recipient: Address::RouterFeeReceiver,
            fee_amount: router_fee + positive_slippage - client_positive_slippage,
        },
        FeeRecipient {
            recipient: Address::ClientFeeReceiver,
            fee_amount: client_fee + client_positive_slippage,
        },
    ])
}

/// Mirrors `FeeCalculator.mustOutputThroughRouter` in Solidity.
///
/// Returns true if funds must pass through the router after the final swap
/// instead of going directly to the receiver.
pub fn must_output_through_router(params: &Params, client_fee_bps: i64) -> Result<bool, Error> {
    let fee_info = _get_fee_info(params)?;

    if fee_info.router_positive_slippage_bps > 0 {
        return Ok(true);
    }
    if client_fee_bps > 0 {
        return Ok(true);
    }
    if fee_info.router_fee_on_output_bps > 0 {
        return Ok(true);
    }
    if fee_info.has_any_token_pair_fee {
        return Ok(true);
    }

    Ok(false)
}

/// Narrows an intermediate 128-bit fee amount to the `i64` fee type, reverting when it does not
/// fit.
fn fee_to_i64(amount: i128) -> Result<i64, Error> {
    i64::try_from(amount).map_err(|_out_of_range| {
        Error::revert(format!("_calculate_fee: fee amount {amount} overflows i64"))
    })
}

/// Mirrors `FeeCalculator._calculateFee` in Solidity.
///
/// Returns `(router_fee, client_fee)`: the total router fee (fee on output +
/// cut of the client fee) and the client's portion of the client fee.
fn _calculate_fee(
    fee_base: i64,
    client_fee_bps: i64,
    fee_info: &FeeInfo,
) -> Result<(i64, i64), Error> {
    if (client_fee_bps + fee_info.router_fee_on_output_bps > MAX_BPS) ||
        fee_info.router_fee_on_client_fee_bps > MAX_BPS
    {
        return Err(Error::revert("_calculate_fee: fee bps too large"));
    }

    let mut router_fee_on_client_fee = 0;
    let mut client_fee = 0;

    if client_fee_bps > 0 {
        let client_fee_numerator = fee_base as i128 * client_fee_bps as i128;
        let total_client_fee = fee_to_i64(client_fee_numerator / MAX_BPS as i128)?;

        if fee_info.router_fee_on_client_fee_bps > 0 {
            router_fee_on_client_fee = fee_to_i64(
                client_fee_numerator * fee_info.router_fee_on_client_fee_bps as i128 /
                    MAX_BPS_SQUARED as i128,
            )?;
        }

        client_fee = checked_subtract(total_client_fee, router_fee_on_client_fee)?;
    }

    let mut router_fee = router_fee_on_client_fee;

    if fee_info.router_fee_on_output_bps > 0 {
        router_fee += fee_to_i64(
            fee_base as i128 * fee_info.router_fee_on_output_bps as i128 / MAX_BPS as i128,
        )?;
    }

    Ok((router_fee, client_fee))
}

/// Mirrors `FeeCalculator._calculatePositiveSlippage` in Solidity.
///
/// Returns the captured part of the positive slippage surplus;
/// zero if the client's router positive slippage bps is zero or there is no surplus.
fn _calculate_positive_slippage(
    actual_amount_out: i64,
    expected_amount_out: i64,
    fee_info: &FeeInfo,
) -> Result<i64, Error> {
    if actual_amount_out <= expected_amount_out {
        return Ok(0);
    }

    fee_to_i64(
        (actual_amount_out - expected_amount_out) as i128 *
            fee_info.router_positive_slippage_bps as i128 /
            MAX_BPS as i128,
    )
}
