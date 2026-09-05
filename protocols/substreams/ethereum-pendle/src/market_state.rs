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
        /// One ABI word: a signed 256-bit big-endian integer, encoded at compile time so the
        /// fixtures below can be written as the numbers the chain reported.
        pub const fn word(value: i64) -> [u8; 32] {
            let mut out = [if value < 0 { 0xff } else { 0x00 }; 32];
            let mut i = 0;
            while i < 8 {
                out[31 - i] = (value as u64 >> (8 * i)) as u8;
                i += 1;
            }
            out
        }

        pub const MARKET: &str = "34280882267ffa6383b363e278b027be083bbe3b";
        pub const YIELD_TOKEN: &str = "04b7fa1e727d7290d6e24fa9b426d0c940283a95";

        /// `keccak("Swap(address,address,int256,int256,uint256,uint256)")`
        pub const SWAP_TOPIC: &str =
            "829000a5bc6a12d46e30cdcecd7c56b1efd88f6d7d059da6734a04f3764557c4";
        /// `keccak("Mint(address,uint256,uint256,uint256)")`
        pub const MINT_TOPIC: &str =
            "b4c03061fb5b7fed76389d5af8f2e0ddb09f8c70d1333abbb62582835e10accb";
        /// `keccak("Burn(address,address,uint256,uint256,uint256)")`
        pub const BURN_TOPIC: &str =
            "4cf25bc1d991c17529c25213d3cc0cda295eeaad5f13f361969b12ea48015f90";
        /// `keccak("UpdateImpliedRate(uint256,uint256)")`
        pub const UPDATE_IMPLIED_RATE_TOPIC: &str =
            "5c0e21d57bb4cf91d8fe238d6f92e2685a695371b19209afcce6217b478f83e1";
        /// `keccak("NewInterestIndex(uint256)")`
        pub const NEW_INTEREST_INDEX_TOPIC: &str =
            "71475f2f645813fdbebf53a58968008bff11ee21a58f01c5a9cc263d0bc4703d";

        /// The Pendle router, padded to a word as an indexed `address`.
        pub const CALLER: &str = "000000000000000000000000888888888889758f76e7103c6cbf23abbf58f946";
        pub const RECEIVER: &str =
            "000000000000000000000000cbc72d92b2dc8187414f6734718563898740c0bc";
        /// `UpdateImpliedRate` indexes the block timestamp, which nothing here decodes.
        pub const IGNORED_TOPIC: &str = CALLER;

        /// Block 25805550. PT moves into the market, so `netPtOut` — reported from the taker's
        /// side — is negative.
        pub const SWAP_NET_PT_OUT: i64 = -170000000000000000;
        pub const SWAP_NET_SY_OUT: i64 = 132950985765988580;
        pub const SWAP_NET_SY_FEE: i64 = 90195456122210;
        /// The protocol's share of `SWAP_NET_SY_FEE`.
        pub const SWAP_NET_SY_TO_RESERVE: i64 = 72156364897767;
        pub const SWAP_DATA: [[u8; 32]; 4] = [
            word(SWAP_NET_PT_OUT),
            word(SWAP_NET_SY_OUT),
            word(SWAP_NET_SY_FEE),
            word(SWAP_NET_SY_TO_RESERVE),
        ];

        /// Block 25784730. `netLpMinted` is the provider's claim on the market, not a reserve.
        pub const MINT_NET_LP_MINTED: i64 = 76842086307550987;
        pub const MINT_NET_SY_USED: i64 = 131007604684081692;
        pub const MINT_NET_PT_USED: i64 = 8050423472964881;
        pub const MINT_DATA: [[u8; 32]; 3] =
            [word(MINT_NET_LP_MINTED), word(MINT_NET_SY_USED), word(MINT_NET_PT_USED)];

        /// Block 25797730. `netLpBurned` is the claim being retired, not a reserve.
        pub const BURN_NET_LP_BURNED: i64 = 1257787386171097418;
        pub const BURN_NET_SY_OUT: i64 = 2144132858120819282;
        pub const BURN_NET_PT_OUT: i64 = 132106885362967758;
        pub const BURN_DATA: [[u8; 32]; 3] =
            [word(BURN_NET_LP_BURNED), word(BURN_NET_SY_OUT), word(BURN_NET_PT_OUT)];

        /// Block 25805550, alongside the swap above.
        pub const IMPLIED_RATE: i64 = 20832594497771595;
        pub const UPDATE_IMPLIED_RATE_DATA: [[u8; 32]; 1] = [word(IMPLIED_RATE)];

        /// Block 25807775. The index is the indexed parameter, so it arrives as a topic and the
        /// data is empty.
        pub const PY_INDEX: i64 = 1242190142783145750;
    }

    fn log(address: &str, topics: &[&str], data: &[[u8; 32]]) -> eth::Log {
        eth::Log {
            address: hex::decode(address).unwrap(),
            topics: topics
                .iter()
                .map(|t| hex::decode(t).unwrap())
                .collect(),
            data: data.concat(),
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
            &fixtures::SWAP_DATA,
        );
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(-fixtures::SWAP_NET_PT_OUT),
                sy: BigInt::from(-(fixtures::SWAP_NET_SY_OUT + fixtures::SWAP_NET_SY_TO_RESERVE)),
            })
        );
    }

    /// `netPtOut` is an `int256`. A swap in the other direction reports it negative, and the
    /// market's PT reserve then falls rather than rises.
    #[test]
    fn swap_direction_follows_the_sign_of_net_pt_out() {
        const PT_LEAVING_THE_MARKET: i64 = -fixtures::SWAP_NET_PT_OUT;

        let mut data = fixtures::SWAP_DATA;
        data[0] = fixtures::word(PT_LEAVING_THE_MARKET);
        let log = log(
            fixtures::MARKET,
            &[fixtures::SWAP_TOPIC, fixtures::CALLER, fixtures::RECEIVER],
            &data,
        );
        assert_eq!(
            reserve_delta(&log)
                .expect("swap did not decode")
                .pt,
            BigInt::from(-PT_LEAVING_THE_MARKET)
        );
    }

    #[test]
    fn mint_adds_what_the_provider_supplied() {
        let log =
            log(fixtures::MARKET, &[fixtures::MINT_TOPIC, fixtures::CALLER], &fixtures::MINT_DATA);
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(fixtures::MINT_NET_PT_USED),
                sy: BigInt::from(fixtures::MINT_NET_SY_USED),
            })
        );
    }

    #[test]
    fn burn_removes_what_the_provider_withdrew() {
        let log = log(
            fixtures::MARKET,
            &[fixtures::BURN_TOPIC, fixtures::CALLER, fixtures::RECEIVER],
            &fixtures::BURN_DATA,
        );
        assert_eq!(
            reserve_delta(&log),
            Some(ReserveDelta {
                pt: BigInt::from(-fixtures::BURN_NET_PT_OUT),
                sy: BigInt::from(-fixtures::BURN_NET_SY_OUT),
            })
        );
    }

    /// `UpdateImpliedRate` fires alongside every reserve event and must not be counted as one.
    #[test]
    fn update_implied_rate_is_not_a_reserve_event() {
        let log = log(
            fixtures::MARKET,
            &[fixtures::UPDATE_IMPLIED_RATE_TOPIC, fixtures::IGNORED_TOPIC],
            &fixtures::UPDATE_IMPLIED_RATE_DATA,
        );
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), Some(BigInt::from(fixtures::IMPLIED_RATE)));
    }

    /// The index is `indexed`, so a decoder reading the empty data would yield zero.
    #[test]
    fn new_interest_index_is_read_from_the_topic() {
        let log = log(
            fixtures::YIELD_TOKEN,
            &[fixtures::NEW_INTEREST_INDEX_TOPIC, &hex::encode(fixtures::word(fixtures::PY_INDEX))],
            &[],
        );
        assert_eq!(py_index_stored(&log), Some(BigInt::from(fixtures::PY_INDEX)));
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), None);
    }

    #[test]
    fn unrelated_logs_are_ignored() {
        let log = log(fixtures::MARKET, &[fixtures::CALLER], &[]);
        assert_eq!(reserve_delta(&log), None);
        assert_eq!(last_ln_implied_rate(&log), None);
        assert_eq!(py_index_stored(&log), None);
    }
}
