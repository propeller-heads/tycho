//! Per-market state derived from market and yield-token events.
//!
//! Every write path on `IPMarket` emits an event, and the events are identical across all five
//! market generations, whereas the storage layout is not. So reserves are replayed from
//! `Swap`/`Mint`/`Burn` rather than read from storage. Markets are created empty and `skim()`
//! moves only donated excess, so a replay from creation reproduces `totalPt`/`totalSy` exactly.

use substreams::scalar::BigInt;
use substreams_ethereum::{pb::eth::v2 as eth, Event};

use crate::abi::{pendle_market, pendle_yield_token};

/// Attribute holding the market's PT reserve.
pub const TOTAL_PT: &str = "total_pt";
/// Attribute holding the market's SY reserve.
pub const TOTAL_SY: &str = "total_sy";
/// Attribute holding `lnLastImpliedRate` as of the market's last interaction.
pub const LAST_LN_IMPLIED_RATE: &str = "last_ln_implied_rate";
/// Attribute holding the yield token's stored PY index.
pub const PY_INDEX_STORED: &str = "py_index_stored";

/// The signed change a single market event makes to the market's reserves.
#[derive(Debug, PartialEq, Eq)]
pub struct ReserveDelta {
    pub pt: BigInt,
    pub sy: BigInt,
}

/// Applies the reserve accounting of one market log, or `None` if the log is not a reserve event.
///
/// `netSyToReserve` is the protocol's cut of the swap fee. It leaves the market's `totalSy`
/// alongside `netSyOut`, so it has to be subtracted too — omitting it overstates the SY reserve
/// by the fee on every swap, compounding over the market's life.
pub fn reserve_delta(log: &eth::Log) -> Option<ReserveDelta> {
    if let Some(event) = pendle_market::events::Swap::match_and_decode(log) {
        return Some(ReserveDelta {
            pt: event.net_pt_out.neg(),
            sy: (event.net_sy_out + event.net_sy_to_reserve).neg(),
        });
    }
    if let Some(event) = pendle_market::events::Mint::match_and_decode(log) {
        return Some(ReserveDelta { pt: event.net_pt_used, sy: event.net_sy_used });
    }
    if let Some(event) = pendle_market::events::Burn::match_and_decode(log) {
        return Some(ReserveDelta { pt: event.net_pt_out.neg(), sy: event.net_sy_out.neg() });
    }
    None
}

/// Reads `lnLastImpliedRate` out of an `UpdateImpliedRate` log.
///
/// The value is absolute — the market recomputes and re-emits it in full on every interaction —
/// so it is carried straight to the attribute with no accumulation.
pub fn last_ln_implied_rate(log: &eth::Log) -> Option<BigInt> {
    pendle_market::events::UpdateImpliedRate::match_and_decode(log)
        .map(|event| event.ln_last_implied_rate)
}

/// Reads the new PY index out of a yield token's `NewInterestIndex` log.
///
/// The parameter is `indexed`, so the value lives in `topics[1]` and the data is empty. A decoder
/// reading the data would silently yield zero.
pub fn py_index_stored(log: &eth::Log) -> Option<BigInt> {
    pendle_yield_token::events::NewInterestIndex::match_and_decode(log).map(|event| event.new_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real mainnet logs from the wstETH market `0x34280882…be3b` and its yield token
    /// `0x04b7fa1e…3a95`. Values were decoded from `eth_getLogs` output; the PY index was
    /// cross-checked against `pyIndexStored()` at the same block.
    mod fixtures {
        pub const MARKET: &str = "34280882267ffa6383b363e278b027be083bbe3b";
        pub const YIELD_TOKEN: &str = "04b7fa1e727d7290d6e24fa9b426d0c940283a95";

        pub const SWAP_TOPIC: &str =
            "829000a5bc6a12d46e30cdcecd7c56b1efd88f6d7d059da6734a04f3764557c4";
        pub const MINT_TOPIC: &str =
            "b4c03061fb5b7fed76389d5af8f2e0ddb09f8c70d1333abbb62582835e10accb";
        pub const BURN_TOPIC: &str =
            "4cf25bc1d991c17529c25213d3cc0cda295eeaad5f13f361969b12ea48015f90";
        pub const UPDATE_IMPLIED_RATE_TOPIC: &str =
            "5c0e21d57bb4cf91d8fe238d6f92e2685a695371b19209afcce6217b478f83e1";
        pub const NEW_INTEREST_INDEX_TOPIC: &str =
            "71475f2f645813fdbebf53a58968008bff11ee21a58f01c5a9cc263d0bc4703d";

        pub const CALLER: &str = "000000000000000000000000888888888889758f76e7103c6cbf23abbf58f946";
        pub const RECEIVER: &str =
            "000000000000000000000000cbc72d92b2dc8187414f6734718563898740c0bc";

        /// Block 25805550: `netPtOut = -1.7e17`, `netSyOut = 1.3295…e17`, `netSyFee = 9.02e13`,
        /// `netSyToReserve = 7.216e13`.
        pub const SWAP_DATA: &str = concat!(
            "fffffffffffffffffffffffffffffffffffffffffffffffffda409e6942f0000",
            "00000000000000000000000000000000000000000000000001d85637971ea8e4",
            "00000000000000000000000000000000000000000000000000005208441f5d62",
            "000000000000000000000000000000000000000000000000000041a0367f7de7",
        );
        /// Block 25784730: `netLpMinted`, `netSyUsed = 1.31e17`, `netPtUsed = 8.05e15`.
        pub const MINT_DATA: &str = concat!(
            "0000000000000000000000000000000000000000000000000110ff786a29570b",
            "00000000000000000000000000000000000000000000000001d16eb8ec11ca1c",
            "000000000000000000000000000000000000000000000000001c99d14765c511",
        );
        /// Block 25797730: `netLpBurned`, `netSyOut = 2.144e18`, `netPtOut = 1.321e17`.
        pub const BURN_DATA: &str = concat!(
            "00000000000000000000000000000000000000000000000011748ef6b3b2014a",
            "0000000000000000000000000000000000000000000000001dc17d796f88d252",
            "00000000000000000000000000000000000000000000000001d55683267164ce",
        );
        /// Block 25805550, alongside the swap above.
        pub const UPDATE_IMPLIED_RATE_DATA: &str =
            "000000000000000000000000000000000000000000000000004a032262ca184b";
        pub const IMPLIED_RATE: i64 = 20832594497771595;

        /// Block 25807775. The index is the indexed parameter, so the data is empty.
        pub const NEW_INTEREST_INDEX_TOPIC1: &str =
            "000000000000000000000000000000000000000000000000113d255a7c14c716";
        pub const PY_INDEX: i64 = 1242190142783145750;
    }

    fn log(address: &str, topics: &[&str], data: &str) -> eth::Log {
        eth::Log {
            address: hex::decode(address).unwrap(),
            topics: topics
                .iter()
                .map(|t| hex::decode(t).unwrap())
                .collect(),
            data: hex::decode(data).unwrap(),
            index: 0,
            block_index: 0,
            ordinal: 0,
        }
    }

    /// The fee split matters: `netSyToReserve` leaves the market on top of `netSyOut`, so the SY
    /// delta is larger in magnitude than the swap's headline output.
    #[test]
    fn swap_moves_both_reserves() {
        let log = log(
            fixtures::MARKET,
            &[fixtures::SWAP_TOPIC, fixtures::CALLER, fixtures::RECEIVER],
            fixtures::SWAP_DATA,
        );
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(170000000000000000_i64),
                sy: BigInt::from(-133023142130886347_i64),
            })
        );
    }

    /// `netPtOut` is an `int256`. A swap in the other direction reports it negative, and the
    /// market's PT reserve then falls rather than rises.
    #[test]
    fn swap_direction_follows_the_sign_of_net_pt_out() {
        let mut data = fixtures::SWAP_DATA.to_string();
        data.replace_range(
            0..64,
            "000000000000000000000000000000000000000000000000025bf6196bd10000",
        );
        let log = log(
            fixtures::MARKET,
            &[fixtures::SWAP_TOPIC, fixtures::CALLER, fixtures::RECEIVER],
            &data,
        );
        assert_eq!(
            reserve_delta(&log)
                .expect("swap did not decode")
                .pt,
            BigInt::from(-170000000000000000_i64)
        );
    }

    #[test]
    fn mint_adds_what_the_provider_supplied() {
        let log =
            log(fixtures::MARKET, &[fixtures::MINT_TOPIC, fixtures::CALLER], fixtures::MINT_DATA);
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(8050423472964881_i64),
                sy: BigInt::from(131007604684081692_i64),
            })
        );
    }

    #[test]
    fn burn_removes_what_the_provider_withdrew() {
        let log = log(
            fixtures::MARKET,
            &[fixtures::BURN_TOPIC, fixtures::CALLER, fixtures::RECEIVER],
            fixtures::BURN_DATA,
        );
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(-132106885362967758_i64),
                sy: BigInt::from(-2144132858120819282_i64),
            })
        );
    }

    /// `UpdateImpliedRate` fires alongside every reserve event and must not be counted as one.
    #[test]
    fn update_implied_rate_is_not_a_reserve_event() {
        let log = log(
            fixtures::MARKET,
            &[fixtures::UPDATE_IMPLIED_RATE_TOPIC, fixtures::CALLER],
            fixtures::UPDATE_IMPLIED_RATE_DATA,
        );
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), Some(BigInt::from(fixtures::IMPLIED_RATE)));
    }

    /// The index is `indexed`, so a decoder reading the empty data would yield zero.
    #[test]
    fn new_interest_index_is_read_from_the_topic() {
        let log = log(
            fixtures::YIELD_TOKEN,
            &[fixtures::NEW_INTEREST_INDEX_TOPIC, fixtures::NEW_INTEREST_INDEX_TOPIC1],
            "",
        );
        assert_eq!(py_index_stored(&log), Some(BigInt::from(fixtures::PY_INDEX)));
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), None);
    }

    #[test]
    fn unrelated_logs_are_ignored() {
        let log = log(fixtures::MARKET, &[fixtures::CALLER], "");
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), None);
        assert_eq!(py_index_stored(&log), None);
    }
}
