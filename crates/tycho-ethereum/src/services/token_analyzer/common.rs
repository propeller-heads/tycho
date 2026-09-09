use std::cmp;

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
/// Returns the higher of the inbound and outbound fee rates. A transfer that credits the
/// receiver with at least the amount sent has no fee. Errors only if a balance plus the amount
/// sent overflows U256.
pub(crate) fn calculate_fee(
    amount: U256,
    middle_amount: U256,
    balance_before_in: U256,
    balance_after_in: U256,
    balance_recipient_before: U256,
    balance_recipient_after: U256,
) -> Result<U256, String> {
    let fee_in = transfer_fee_bps(amount, balance_before_in, balance_after_in)?;
    let fee_out =
        transfer_fee_bps(middle_amount, balance_recipient_before, balance_recipient_after)?;
    Ok(cmp::max(fee_in, fee_out))
}

/// Fee in basis points that one transfer of `sent` took, from the receiver's balance before
/// and after. Zero when nothing was sent or the receiver got at least `sent`.
fn transfer_fee_bps(sent: U256, before: U256, after: U256) -> Result<U256, String> {
    let expected = before
        .checked_add(sent)
        .ok_or_else(|| format!("balance {before} + {sent} overflows"))?;
    if sent.is_zero() || after >= expected {
        return Ok(U256::ZERO);
    }
    let shortfall = expected - after;
    let scaled = shortfall
        .checked_mul(U256::from(10_000))
        .ok_or_else(|| format!("shortfall {shortfall} * 10_000 overflows"))?;
    Ok(scaled / sent)
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

#[cfg(test)]
mod tests {
    use alloy::{primitives::U256, rpc::types::BlockNumberOrTag};
    use tycho_common::models::blockchain::BlockTag;

    use super::{calculate_fee, map_block_tag};

    fn fee(
        amount: u64,
        before_in: u64,
        after_in: u64,
        recipient_before: u64,
        recipient_after: u64,
    ) -> Result<U256, String> {
        let after_in = U256::from(after_in);
        let before_in = U256::from(before_in);
        calculate_fee(
            U256::from(amount),
            after_in - before_in,
            before_in,
            after_in,
            U256::from(recipient_before),
            U256::from(recipient_after),
        )
    }

    #[test]
    fn calculate_fee_no_fee() {
        assert_eq!(fee(1_000_000, 0, 1_000_000, 0, 1_000_000), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_one_percent() {
        assert_eq!(fee(1_000_000, 0, 990_000, 0, 980_100), Ok(U256::from(100)));
    }

    #[test]
    fn calculate_fee_settlement_dust_above_fee() {
        assert_eq!(fee(1_000_000, 50_000, 1_040_000, 0, 990_000), Ok(U256::from(100)));
    }

    #[test]
    fn calculate_fee_rounding_token_with_settlement_dust() {
        assert_eq!(fee(1_000_000, 2, 1_000_001, 0, 999_999), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_bonus_token() {
        assert_eq!(fee(1_000_000, 0, 1_000_001, 0, 1_000_001), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_takes_the_higher_leg() {
        assert_eq!(fee(1_000_000, 0, 990_000, 0, 792_000), Ok(U256::from(2_000)));
    }

    #[test]
    fn calculate_fee_full_fee() {
        assert_eq!(fee(1_000_000, 7, 7, 0, 0), Ok(U256::from(10_000)));
    }

    #[test]
    fn calculate_fee_balance_near_max_errors() {
        let result = calculate_fee(
            U256::from(1_000_000),
            U256::ZERO,
            U256::MAX,
            U256::MAX,
            U256::ZERO,
            U256::ZERO,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_map_block_tag() {
        assert_eq!(map_block_tag(BlockTag::Finalized), BlockNumberOrTag::Finalized);
        assert_eq!(map_block_tag(BlockTag::Safe), BlockNumberOrTag::Safe);
        assert_eq!(map_block_tag(BlockTag::Latest), BlockNumberOrTag::Latest);
        assert_eq!(map_block_tag(BlockTag::Earliest), BlockNumberOrTag::Earliest);
        assert_eq!(map_block_tag(BlockTag::Pending), BlockNumberOrTag::Pending);
        assert_eq!(map_block_tag(BlockTag::Number(123)), BlockNumberOrTag::Number(123));
    }
}
