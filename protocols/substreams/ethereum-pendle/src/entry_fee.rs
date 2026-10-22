//! The entry fee one vault charges outside US equity market hours, followed from its events onto
//! the SYs whose deposits pay it. See [`crate::consts::ENTRY_FEE_VAULT`].

use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2 as eth;

use crate::consts::{ENTRY_FEE_NEXT_CLOSE, ENTRY_FEE_SET, ENTRY_FEE_VAULT};

/// State attribute: the entry fee in basis points the SY's `index_rate` deposits pay while it
/// applies.
pub const ENTRY_FEE_BPS: &str = "entry_fee_bps";
/// State attribute: the timestamp from which that fee applies, until the vault moves it.
pub const ENTRY_FEE_FROM: &str = "entry_fee_from";

/// Key under which `store_entry_fee` keeps the fee in basis points.
pub const FEE_BPS_KEY: &str = "entry_fee:bps";
/// Key under which `store_entry_fee` keeps the timestamp the fee applies from.
pub const FEE_FROM_KEY: &str = "entry_fee:from";
/// Key under which `store_entry_fee_payers` lists the SYs that pay it.
pub const PAYERS_KEY: &str = "entry_fee:payers";

/// One change the vault announces: the fee, or the next close from which it applies.
#[derive(Debug, PartialEq, Eq)]
pub enum FeeChange {
    Bps(BigInt),
    From(BigInt),
}

/// The change `log` announces, if it is one of the vault's two fee events.
pub fn fee_change(log: &eth::Log) -> Option<FeeChange> {
    if log.address != ENTRY_FEE_VAULT || log.topics.len() != 1 || log.data.len() != 32 {
        return None;
    }
    let value = BigInt::from_unsigned_bytes_be(&log.data);
    match log.topics[0].as_slice() {
        topic if topic == ENTRY_FEE_SET => Some(FeeChange::Bps(value)),
        topic if topic == ENTRY_FEE_NEXT_CLOSE => Some(FeeChange::From(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(address: [u8; 20], topic0: [u8; 32], word: u64) -> eth::Log {
        let mut data = vec![0u8; 24];
        data.extend_from_slice(&word.to_be_bytes());
        eth::Log {
            address: address.to_vec(),
            topics: vec![topic0.to_vec()],
            data,
            ..Default::default()
        }
    }

    /// The vault set its fee to 25 bp at block 26,091,045, and at 13:32:47 UTC on 2026-10-05 moved
    /// the next close to 20:00:00 UTC that day.
    #[test]
    fn the_vault_announces_its_fee_and_its_next_close() {
        assert_eq!(
            fee_change(&log(ENTRY_FEE_VAULT, ENTRY_FEE_SET, 25)),
            Some(FeeChange::Bps(BigInt::from(25)))
        );
        assert_eq!(
            fee_change(&log(ENTRY_FEE_VAULT, ENTRY_FEE_NEXT_CLOSE, 1_791_230_400)),
            Some(FeeChange::From(BigInt::from(1_791_230_400u64)))
        );
        assert_eq!(fee_change(&log([0x11; 20], ENTRY_FEE_SET, 25)), None);
    }
}
