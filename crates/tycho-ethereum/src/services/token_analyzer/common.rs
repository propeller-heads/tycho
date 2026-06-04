use alloy::{
    primitives::{keccak256, Address, U256},
    rpc::types::{BlockNumberOrTag, TransactionInput, TransactionRequest},
};
use tycho_common::models::blockchain::BlockTag;

/// Returns a deterministic address with no token balance, used as the transfer-out recipient.
/// An address that never holds tokens catches exemptions some tokens grant to known addresses
/// (e.g. their own Uniswap pools).
pub(crate) fn arbitrary_recipient() -> Address {
    let hash = keccak256(b"propeller");
    Address::from_slice(&hash[..20])
}

/// Computes the transfer fee in basis points (0–10_000) from observed balance deltas.
///
/// Returns the higher of the inbound and outbound fee rates. Returns zero if neither transfer
/// shows a fee. Errors only on arithmetic overflow, which is not expected in practice.
pub(crate) fn calculate_fee(
    amount: U256,
    middle_amount: U256,
    balance_before_in: U256,
    balance_after_in: U256,
    balance_recipient_before: U256,
    balance_recipient_after: U256,
) -> Result<U256, String> {
    Ok(
        match (
            balance_after_in != error_add(balance_before_in, amount)?,
            balance_recipient_after != error_add(balance_recipient_before, middle_amount)?,
        ) {
            (true, true) => {
                let first_transfer_fees = error_div(
                    error_mul(
                        error_sub(error_add(balance_before_in, amount)?, balance_after_in)?,
                        U256::from(10_000),
                    )?,
                    amount,
                )?;
                let second_transfer_fees = error_div(
                    error_mul(
                        error_sub(
                            error_add(balance_recipient_before, middle_amount)?,
                            balance_recipient_after,
                        )?,
                        U256::from(10_000),
                    )?,
                    middle_amount,
                )?;
                if first_transfer_fees >= second_transfer_fees {
                    first_transfer_fees
                } else {
                    second_transfer_fees
                }
            }
            (true, false) => error_div(
                error_mul(
                    error_sub(error_add(balance_before_in, amount)?, balance_after_in)?,
                    U256::from(10_000),
                )?,
                amount,
            )?,
            (false, true) => error_div(
                error_mul(
                    error_sub(
                        error_add(balance_recipient_before, middle_amount)?,
                        balance_recipient_after,
                    )?,
                    U256::from(10_000),
                )?,
                middle_amount,
            )?,
            (false, false) => U256::ZERO,
        },
    )
}

/// Converts a tycho BlockTag to an alloy BlockNumberOrTag.
pub(crate) fn map_block_tag(block: BlockTag) -> BlockNumberOrTag {
    match block {
        BlockTag::Finalized => BlockNumberOrTag::Finalized,
        BlockTag::Safe => BlockNumberOrTag::Safe,
        BlockTag::Latest => BlockNumberOrTag::Latest,
        BlockTag::Earliest => BlockNumberOrTag::Earliest,
        BlockTag::Pending => BlockNumberOrTag::Pending,
        BlockTag::Number(n) => BlockNumberOrTag::Number(n),
    }
}

/// Builds a `TransactionRequest` for a read-only or impersonated call used in trace simulations.
pub(crate) fn call_request(
    from: Option<Address>,
    to: Address,
    calldata: Vec<u8>,
) -> TransactionRequest {
    let mut req = TransactionRequest::default()
        .to(to)
        .input(TransactionInput::both(calldata.into()));

    if let Some(addr) = from {
        req = req.from(addr);
    }

    req
}

fn error_add(a: U256, b: U256) -> Result<U256, String> {
    a.checked_add(b)
        .ok_or_else(|| "overflow".to_string())
}

fn error_sub(a: U256, b: U256) -> Result<U256, String> {
    a.checked_sub(b)
        .ok_or_else(|| "overflow".to_string())
}

fn error_div(a: U256, b: U256) -> Result<U256, String> {
    a.checked_div(b)
        .ok_or_else(|| "overflow".to_string())
}

fn error_mul(a: U256, b: U256) -> Result<U256, String> {
    a.checked_mul(b)
        .ok_or_else(|| "overflow".to_string())
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::U256, rpc::types::BlockNumberOrTag};
    use tycho_common::models::blockchain::BlockTag;

    use super::{calculate_fee, map_block_tag};

    #[test]
    fn test_map_block_tag() {
        assert_eq!(map_block_tag(BlockTag::Finalized), BlockNumberOrTag::Finalized);
        assert_eq!(map_block_tag(BlockTag::Safe), BlockNumberOrTag::Safe);
        assert_eq!(map_block_tag(BlockTag::Latest), BlockNumberOrTag::Latest);
        assert_eq!(map_block_tag(BlockTag::Earliest), BlockNumberOrTag::Earliest);
        assert_eq!(map_block_tag(BlockTag::Pending), BlockNumberOrTag::Pending);
        assert_eq!(map_block_tag(BlockTag::Number(123)), BlockNumberOrTag::Number(123));
    }

    #[test]
    fn calculate_fee_does_not_underflow_when_settlement_holds_prior_balance() {
        // Fee-on-transfer token where the settlement already holds more than the per-transfer
        // fee. The taxed inbound transfer pushes the settlement balance above `amount`, so the
        // fee must add the prior balance before subtracting; the old code computed
        // `amount - balance_after_in` first, which underflows here.
        let amount = U256::from(100_000u64);
        let balance_before_in = U256::from(5_000u64);
        // 2% inbound fee: 98_000 received on top of the prior 5_000 balance.
        let balance_after_in = U256::from(103_000u64);
        let middle_amount = balance_after_in - balance_before_in;
        // Outbound leg takes no extra fee: recipient starts at 0 and receives all of it.
        let recipient_before = U256::ZERO;
        let recipient_after = middle_amount;

        let fee = calculate_fee(
            amount,
            middle_amount,
            balance_before_in,
            balance_after_in,
            recipient_before,
            recipient_after,
        )
        .expect("fee calculation must not underflow when settlement holds a prior balance");

        assert_eq!(fee, U256::from(200u64), "2% inbound fee should be 200 bps");
    }

    #[test]
    fn calculate_fee_matches_basis_points_with_zero_prior_balance() {
        // Control case: settlement holds nothing beforehand. A 1% inbound fee on 1_000_000
        // leaves 990_000 and must still report 100 bps after the fix.
        let amount = U256::from(1_000_000u64);
        let balance_before_in = U256::ZERO;
        let balance_after_in = U256::from(990_000u64);
        let middle_amount = balance_after_in - balance_before_in;
        let recipient_before = U256::ZERO;
        let recipient_after = middle_amount;

        let fee = calculate_fee(
            amount,
            middle_amount,
            balance_before_in,
            balance_after_in,
            recipient_before,
            recipient_after,
        )
        .expect("fee calculation should succeed for the zero prior balance case");

        assert_eq!(fee, U256::from(100u64), "1% inbound fee should be 100 bps");
    }
}
