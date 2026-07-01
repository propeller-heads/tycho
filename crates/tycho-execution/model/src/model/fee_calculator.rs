//! <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/FeeCalculator.sol>
use crate::{address::Address, error::Error, math::checked_subtract, params::Params};

pub const MAX_BPS: i64 = 100_000_000;

/// <https://github.com/propeller-heads/tycho-execution/blob/9b0512c9580617224c7a0d7de781674a2cdc6b62/foundry/lib/FeeStructs.sol#L9>
pub struct FeeRecipient {
    pub recipient: Address,
    pub fee_amount: i64,
}

struct FeeInfo {
    router_fee_on_output_bps: i64,
    router_fee_on_client_fee_bps: i64,
    positive_slippage_enabled: bool,
    client_slippage_share_bps: i64,
}

fn _get_fee_info(params: &Params) -> Result<FeeInfo, Error> {
    let (router_fee_on_output_bps, router_fee_on_client_fee_bps) =
        if crate::config::ENABLE_NONZERO_FEE_BPS {
            let has_client_custom_fee_on_input =
                params.request("has_client_custom_fee_on_input", vec![true, false])?;

            let router_fee_on_output_bps = if has_client_custom_fee_on_input {
                params.request("client_fee_bps_on_output", vec![0, MAX_BPS])?
            } else {
                params.request("router_fee_on_output_bps", vec![0, MAX_BPS])?
            };

            let has_client_custom_fee_on_client_fee =
                params.request("has_client_custom_fee_on_client_fee", vec![true, false])?;
            let router_fee_on_client_fee_bps = if has_client_custom_fee_on_client_fee {
                params.request("client_fee_bps_on_client_fee", vec![0, MAX_BPS])?
            } else {
                params.request("router_fee_on_client_fee_bps", vec![0, MAX_BPS])?
            };

            (router_fee_on_output_bps, router_fee_on_client_fee_bps)
        } else {
            (0, 0)
        };

    let (positive_slippage_enabled, client_slippage_share_bps) =
        if crate::config::ENABLE_POSITIVE_SLIPPAGE {
            let enabled = params.request("positive_slippage_enabled", vec![true, false])?;
            let has_custom_client_slippage_share =
                params.request("has_custom_client_slippage_share", vec![true, false])?;
            let client_slippage_share_bps = if has_custom_client_slippage_share {
                params.request("custom_client_slippage_share_bps", vec![0, MAX_BPS])?
            } else {
                params.request("default_client_slippage_share_bps", vec![0, MAX_BPS])?
            };
            (enabled, client_slippage_share_bps)
        } else {
            (false, 0)
        };

    Ok(FeeInfo {
        router_fee_on_output_bps,
        router_fee_on_client_fee_bps,
        positive_slippage_enabled,
        client_slippage_share_bps,
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
) -> Result<Vec<FeeRecipient>, Error> {
    let fee_info = _get_fee_info(params)?;

    let slippage = _calculate_positive_slippage(actual_amount_out, expected_amount_out, &fee_info);

    let mut fee_base = actual_amount_out;
    if !slippage.is_empty() {
        fee_base -= slippage[0].fee_amount + slippage[1].fee_amount;
    }

    let fees = _calculate_fee(fee_base, client_fee_bps, &fee_info)?;

    Ok(_merge_fee_recipients(fees, slippage))
}

/// Mirrors `FeeCalculator.mustInterceptOutput` in Solidity.
///
/// Returns true if funds must pass through the router after the final swap
/// instead of going directly to the receiver.
pub fn must_intercept_output(params: &Params, client_fee_bps: i64) -> Result<bool, Error> {
    let fee_info = _get_fee_info(params)?;

    if fee_info.positive_slippage_enabled {
        return Ok(true);
    }
    if client_fee_bps > 0 {
        return Ok(true);
    }
    if fee_info.router_fee_on_output_bps > 0 {
        return Ok(true);
    }

    Ok(false)
}

/// Mirrors `FeeCalculator._calculateFee` in Solidity.
///
/// Returns 2-element array: [0] = router, [1] = client.
fn _calculate_fee(
    fee_base: i64,
    client_fee_bps: i64,
    fee_info: &FeeInfo,
) -> Result<Vec<FeeRecipient>, Error> {
    if (client_fee_bps + fee_info.router_fee_on_output_bps > MAX_BPS) ||
        fee_info.router_fee_on_client_fee_bps > MAX_BPS
    {
        return Err(Error::revert("_calculate_fee: fee bps too large"));
    }

    let mut router_fee_on_client_fee = 0;
    let mut client_portion = 0;

    if client_fee_bps > 0 {
        let client_fee_numerator = fee_base * client_fee_bps;
        let total_client_fee = client_fee_numerator / MAX_BPS;

        if fee_info.router_fee_on_client_fee_bps > 0 {
            router_fee_on_client_fee =
                client_fee_numerator * fee_info.router_fee_on_client_fee_bps / (MAX_BPS * MAX_BPS);
        }

        client_portion = checked_subtract(total_client_fee, router_fee_on_client_fee)?;
    }

    let mut total_router_fee = router_fee_on_client_fee;

    if fee_info.router_fee_on_output_bps > 0 {
        total_router_fee += fee_base * fee_info.router_fee_on_output_bps / MAX_BPS;
    }

    Ok(vec![
        FeeRecipient { recipient: Address::RouterFeeReceiver, fee_amount: total_router_fee },
        FeeRecipient { recipient: Address::ClientFeeReceiver, fee_amount: client_portion },
    ])
}

/// Mirrors `FeeCalculator._calculatePositiveSlippage` in Solidity.
///
/// Returns empty vec if disabled or no surplus; otherwise 2-element vec:
/// [0] = router, [1] = client.
fn _calculate_positive_slippage(
    actual_amount_out: i64,
    expected_amount_out: i64,
    fee_info: &FeeInfo,
) -> Vec<FeeRecipient> {
    if !fee_info.positive_slippage_enabled || actual_amount_out <= expected_amount_out {
        return vec![];
    }

    let surplus = actual_amount_out - expected_amount_out;
    let client_cut = surplus * fee_info.client_slippage_share_bps / MAX_BPS;
    let router_cut = surplus - client_cut;

    vec![
        FeeRecipient { recipient: Address::RouterFeeReceiver, fee_amount: router_cut },
        FeeRecipient { recipient: Address::ClientFeeReceiver, fee_amount: client_cut },
    ]
}

/// Mirrors `FeeCalculator._mergeFeeRecipients` in Solidity.
///
/// Merges slippage amounts into fee amounts for the same recipients.
fn _merge_fee_recipients(
    mut fees: Vec<FeeRecipient>,
    slippage: Vec<FeeRecipient>,
) -> Vec<FeeRecipient> {
    if slippage.is_empty() {
        return fees;
    }

    // fees[0] = router fees, slippage[0] = router slippage
    // fees[1] = client fees, slippage[1] = client slippage
    debug_assert!(fees.len() == 2, "expected exactly 2 fee recipients (router, client)");
    debug_assert!(slippage.len() == 2, "expected exactly 2 slippage recipients (router, client)");
    fees[0].fee_amount += slippage[0].fee_amount;
    fees[1].fee_amount += slippage[1].fee_amount;

    fees
}
