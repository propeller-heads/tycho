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

/// One simulated token transfer, seen through the receiver's balance.
pub(crate) struct ObservedTransfer {
    /// Amount the sender transferred.
    pub(crate) sent: U256,
    /// Receiver balance before the transfer.
    pub(crate) balance_before: U256,
    /// Receiver balance after the transfer.
    pub(crate) balance_after: U256,
}

impl ObservedTransfer {
    /// Computes the fee in basis points this transfer took from the receiver's balance change.
    /// Zero when nothing was sent or the receiver got at least `sent`. Above 10_000 when the
    /// receiver ends with less than it started. Errors if `balance_before + sent` or
    /// `shortfall * 10_000` overflows U256.
    fn fee_bps(&self) -> Result<U256, String> {
        let expected_after = self
            .balance_before
            .checked_add(self.sent)
            .ok_or_else(|| format!("balance {} + {} overflows", self.balance_before, self.sent))?;
        if self.sent.is_zero() || self.balance_after >= expected_after {
            return Ok(U256::ZERO);
        }
        // Safe: the guard above returned unless balance_after < expected_after.
        let shortfall = expected_after - self.balance_after;
        Ok(shortfall
            .checked_mul(U256::from(10_000))
            .ok_or_else(|| format!("shortfall {shortfall} * 10_000 overflows"))? /
            self.sent)
    }
}

/// Computes the transfer fee in basis points from the two simulated transfers: `inbound` moves
/// `amount` from a holder into the settlement contract, `outbound` moves what arrived on to the
/// recipient.
///
/// Returns the higher of the two fee rates. A transfer that credits the receiver with at least
/// the amount sent has no fee. The result exceeds 10_000 when a receiver ends with less than it
/// started. Errors if `balance_before + sent` or `shortfall * 10_000` overflows U256.
pub(crate) fn calculate_fee_bps(
    inbound: ObservedTransfer,
    outbound: ObservedTransfer,
) -> Result<U256, String> {
    Ok(inbound
        .fee_bps()?
        .max(outbound.fee_bps()?))
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

    use super::{calculate_fee_bps, map_block_tag, ObservedTransfer};

    // Builds the two transfers the way the detectors do: everything the settlement received
    // (`after_in - before_in`) is sent on to the recipient.
    fn fee(
        amount: u64,
        before_in: u64,
        after_in: u64,
        recipient_before: u64,
        recipient_after: u64,
    ) -> Result<U256, String> {
        let before_in = U256::from(before_in);
        let after_in = U256::from(after_in);
        calculate_fee_bps(
            ObservedTransfer {
                sent: U256::from(amount),
                balance_before: before_in,
                balance_after: after_in,
            },
            ObservedTransfer {
                sent: after_in - before_in,
                balance_before: U256::from(recipient_before),
                balance_after: U256::from(recipient_after),
            },
        )
    }

    #[test]
    fn calculate_fee_bps_no_fee() {
        assert_eq!(fee(1_000_000, 0, 1_000_000, 0, 1_000_000), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_bps_one_percent() {
        assert_eq!(fee(1_000_000, 0, 990_000, 0, 980_100), Ok(U256::from(100)));
    }

    #[test]
    fn calculate_fee_bps_settlement_balance_exceeds_fee() {
        assert_eq!(fee(1_000_000, 50_000, 1_040_000, 0, 990_000), Ok(U256::from(100)));
    }

    #[test]
    fn calculate_fee_bps_one_wei_short_is_zero() {
        assert_eq!(fee(1_000_000, 2, 1_000_001, 0, 999_999), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_bps_credits_more_than_sent_is_zero() {
        assert_eq!(fee(1_000_000, 0, 1_000_001, 0, 1_000_001), Ok(U256::ZERO));
    }

    #[test]
    fn calculate_fee_bps_takes_the_higher_rate() {
        // 1% inbound, 20% outbound of the 990_000 that arrived.
        assert_eq!(fee(1_000_000, 0, 990_000, 0, 792_000), Ok(U256::from(2_000)));
    }

    #[test]
    fn calculate_fee_bps_full_fee() {
        // Nothing arrived, so nothing is sent on.
        assert_eq!(fee(1_000_000, 7, 7, 0, 0), Ok(U256::from(10_000)));
    }

    #[test]
    fn calculate_fee_bps_balance_near_max_errors() {
        let result = calculate_fee_bps(
            ObservedTransfer {
                sent: U256::from(1_000_000),
                balance_before: U256::MAX,
                balance_after: U256::MAX,
            },
            ObservedTransfer {
                sent: U256::ZERO,
                balance_before: U256::ZERO,
                balance_after: U256::ZERO,
            },
        );
        assert!(result
            .unwrap_err()
            .ends_with("+ 1000000 overflows"));
    }

    #[test]
    fn calculate_fee_bps_shortfall_near_max_errors() {
        let result = calculate_fee_bps(
            ObservedTransfer {
                sent: U256::MAX,
                balance_before: U256::ZERO,
                balance_after: U256::ZERO,
            },
            ObservedTransfer {
                sent: U256::ZERO,
                balance_before: U256::ZERO,
                balance_after: U256::ZERO,
            },
        );
        assert!(result
            .unwrap_err()
            .ends_with("* 10_000 overflows"));
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
