//! What a direction may publish where its output is lent by a venue that holds a partial repay at
//! a minimum position (Lista Moolah's). The pool passes such a venue over where a draw would leave
//! its debt under the minimum, or within the release slack over it where it already owes, and the
//! quote then refuses. With F what the venues free of the output before a draw (its custody,
//! supply and posted collateral) and E = F + minimum - owed (plus the slack where it owes), a
//! payout past custody and under E can refuse, while one within custody, or at or over E, passes.
//! No probe need land in that window, and a chord or a flat span over it quotes payouts that
//! refuse, so the ladder keeps one piece: the inputs paid from custody, or those paying at least E.
use std::ops::Range;

use alloy::primitives::U256;

use super::{
    gas::{with_margin, Legs, Minimum, Venue, Venues, RELEASE_SLACK},
    ladder::{Probe, ProbeStatus, Rung},
};

const WAD: u64 = 1_000_000_000_000_000_000;
/// The least band the protocol's part is bounded by (`MIN_FEE_BAND_WAD`).
const MIN_BAND_WAD: u64 = 10_000_000_000_000_000;

/// What a direction reads of the live venue that lends its output and holds a minimum position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeldRepay {
    pub minimum: Minimum,
    /// It is the only live venue that lends the output.
    pub sole: bool,
}

impl HeldRepay {
    /// The guard of a direction whose output is `token_out`, where a live venue holding a minimum
    /// position lends it; `None` where none does.
    pub(super) fn lending(venues: &Venues, token_out: u8) -> Option<Self> {
        let lenders: Vec<&Venue> = venues
            .records
            .iter()
            .filter(|v| v.live && v.debt == token_out)
            .collect();
        let minimum = lenders
            .iter()
            .find_map(|v| v.held_repay)?;
        Some(Self { minimum, sole: lenders.len() == 1 })
    }
}

/// The most the protocol's part and the operator's share add to a payout of `y` priced at label
/// `f` in a band of `band`, plus one unit: the larger of the label's fee, the band's bound at the
/// highest spread rate and the notional floor, each rounded up. `None` where the label is not a
/// fee under one.
fn parts(y: U256, f: U256, band: U256) -> Option<U256> {
    let wad = U256::from(WAD);
    let fee = y.checked_mul(f)?.div_ceil(
        wad.checked_sub(f)
            .filter(|d| !d.is_zero())?,
    );
    let bound = y
        .checked_mul(band.max(U256::from(MIN_BAND_WAD)))?
        .checked_mul(U256::from(3u64))?
        .div_ceil(U256::from(10_000_000_000_000_000_000u128));
    let floor = y.div_ceil(U256::from(1_000u64));
    fee.max(bound)
        .max(floor)
        .checked_add(U256::ONE)
}

/// The rungs of a settled ladder, on the pool's own outputs before any haircut, that a direction
/// whose output `guard`'s venue lends may publish: all of them, none, or one piece. `legs` are the
/// pool's positions as a walk of the pair read them at this block, the input's first, and `band`
/// that walk's band word; `None` where no walk of the pair read them, and nothing is published.
/// `probes` give the label each rung is priced at, the largest at or under its input, and the
/// pool's own answer at its input.
///
/// The low piece is the rungs whose payout and parts fit the output's custody, and the high piece
/// those from the first paying at least E, the minimum taken with its margin. Where the minimum
/// did not read, or another venue lends the output beside it, only the low piece is kept: a draw
/// split between venues can leave the minimum's slice under it. Where E is not past F, every draw
/// passes the minimum and the ladder stands. Otherwise the piece whose top pays more is kept.
pub(super) fn keep(
    rungs: &[Rung],
    probes: &[Probe],
    legs: Option<&Legs>,
    band: U256,
    guard: HeldRepay,
) -> Range<usize> {
    let Some(legs) = legs else { return 0..0 };
    let [_, _, _, _, held, supplied, posted, owed] = *legs;
    let low = rungs
        .iter()
        .take_while(|rung| {
            let priced = probes
                .iter()
                .filter(|p| matches!(p.status, ProbeStatus::Priced | ProbeStatus::Clipped));
            let label = priced
                .clone()
                .filter(|p| p.used <= rung.input)
                .map(|p| p.fee_wad)
                .max()
                .unwrap_or_default();
            let paid = priced
                .filter(|p| p.used == rung.input)
                .map(|p| p.out)
                .fold(rung.output, U256::max);
            parts(paid, label, band)
                .and_then(|parts| paid.checked_add(parts))
                .is_some_and(|need| need <= held)
        })
        .count();
    let (Minimum::Read(minimum), true) = (guard.minimum, guard.sole) else {
        return 0..low;
    };
    let freed = held
        .saturating_add(supplied)
        .saturating_add(posted);
    let slack = if owed.is_zero() { U256::ZERO } else { U256::from(RELEASE_SLACK) };
    let edge = freed
        .saturating_add(with_margin(minimum))
        .saturating_add(slack)
        .saturating_sub(owed);
    if edge <= freed || low == rungs.len() {
        return 0..rungs.len();
    }
    match rungs
        .iter()
        .position(|rung| rung.output >= edge)
    {
        Some(start) => start..rungs.len(),
        None => 0..low,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{b256, keccak256};
    use serde_json::Value;

    use super::*;
    use crate::pamm::protocols::flamm::{gas::tests::venue, ladder::Segment};

    /// The guard's shared vectors, the same bytes in every client that serves these pools.
    const VECTORS: &str = include_str!("testdata/held_repay_guard_vectors.json");

    fn number(value: &Value) -> U256 {
        value
            .as_str()
            .expect("a vector's numbers are decimal strings")
            .parse()
            .expect("a decimal number")
    }

    fn rung(input: u64, output: u64) -> Rung {
        Rung { input: U256::from(input), output: U256::from(output), segment: Segment::Chord }
    }

    fn priced(used: u64, out: u64, fee_wad: u64) -> Probe {
        Probe {
            ask: U256::from(used),
            used: U256::from(used),
            out: U256::from(out),
            fee_wad: U256::from(fee_wad),
            status: ProbeStatus::Priced,
        }
    }

    /// The output holds 1,000,000, its venue frees 500,000 more of it, and it owes `owed_out`.
    fn legs(owed_out: u64) -> Legs {
        let mut legs = [U256::ZERO; 8];
        legs[4] = U256::from(1_000_000u64);
        legs[5] = U256::from(500_000u64);
        legs[7] = U256::from(owed_out);
        legs
    }

    /// Four rungs paying 500,000, 990,000, 1,600,000 and 3,000,000, priced at 0.3 %.
    fn ladder() -> (Vec<Rung>, Vec<Probe>) {
        let points = [(10, 500_000), (20, 990_000), (40, 1_600_000), (80, 3_000_000)];
        (
            points
                .iter()
                .map(|(x, y)| rung(*x, *y))
                .collect(),
            points
                .iter()
                .map(|(x, y)| priced(*x, *y, 3_000_000_000_000_000))
                .collect(),
        )
    }

    fn sole(minimum: u64) -> HeldRepay {
        HeldRepay { minimum: Minimum::Read(U256::from(minimum)), sole: true }
    }

    /// E is 1,500,000 and a minimum of 1,000,000 with its margin, 2,510,000: only the top rung pays
    /// that. With a minimum of 5,000,000 no rung does, and the two whose payouts fit custody with
    /// their parts stay: 990,000 and 2,980 at 0.3 % in the 1 % band.
    #[test]
    fn a_window_past_custody_keeps_the_rungs_paying_past_it() {
        let (rungs, probes) = ladder();
        assert_eq!(keep(&rungs, &probes, Some(&legs(0)), U256::ZERO, sole(1_000_000)), 3..4);
        assert_eq!(keep(&rungs, &probes, Some(&legs(0)), U256::ZERO, sole(5_000_000)), 0..2);
    }

    /// A debt at the minimum with its margin and the slack leaves no window; one unit less leaves
    /// a window one unit wide past what the venues free, which the two top rungs pay past.
    #[test]
    fn a_debt_over_the_minimum_leaves_no_window() {
        let (rungs, probes) = ladder();
        let at = 1_010_000 + 4;
        assert_eq!(keep(&rungs, &probes, Some(&legs(at)), U256::ZERO, sole(1_000_000)), 0..4);
        assert_eq!(keep(&rungs, &probes, Some(&legs(at - 1)), U256::ZERO, sole(1_000_000)), 2..4);
    }

    #[test]
    fn an_unread_minimum_or_a_second_lender_keeps_the_custody_rungs_alone() {
        let (rungs, probes) = ladder();
        for guard in [
            HeldRepay { minimum: Minimum::Unread, sole: true },
            HeldRepay { minimum: Minimum::Read(U256::from(1_000_000u64)), sole: false },
        ] {
            assert_eq!(keep(&rungs, &probes, Some(&legs(1_010_004)), U256::ZERO, guard), 0..2);
        }
    }

    #[test]
    fn unread_legs_publish_nothing_and_a_ladder_within_custody_stands() {
        let (rungs, probes) = ladder();
        assert_eq!(keep(&rungs, &probes, None, U256::ZERO, sole(1_000_000)), 0..0);
        let mut rich = legs(0);
        rich[4] = U256::from(10_000_000u64);
        assert_eq!(keep(&rungs, &probes, Some(&rich), U256::ZERO, sole(1_000_000)), 0..4);
    }

    /// A rung's payout is the larger of its output and the pool's own answer at its input, and its
    /// label the largest at or under its input.
    #[test]
    fn a_rung_is_checked_at_the_pools_own_answer_and_the_largest_label_under_it() {
        let (rungs, mut probes) = ladder();
        let mut rich = legs(0);
        rich[4] = U256::from(3_100_000u64);
        assert_eq!(keep(&rungs, &probes, Some(&rich), U256::ZERO, sole(1_000_000)), 0..4);
        probes[3].out = U256::from(3_100_000u64);
        assert_eq!(keep(&rungs, &probes, Some(&rich), U256::ZERO, sole(1_000_000)), 0..3);
        let (rungs, mut probes) = ladder();
        probes[0].fee_wad = U256::from(40_000_000_000_000_000u64);
        // 4 % at the first rung: 990,000 pays 41,250 of parts, past 1,030,000
        let mut snug = legs(0);
        snug[4] = U256::from(1_030_000u64);
        assert_eq!(keep(&rungs, &probes, Some(&snug), U256::ZERO, sole(5_000_000)), 0..1);
    }

    /// The parts are bounded by the largest of the label's fee, three tenths of the band (at least
    /// 1 %) and a thousandth of the payout, each rounded up, plus one.
    #[test]
    fn the_parts_take_the_largest_bound() {
        let y = U256::from(1_000_000u64);
        // 1e6 * 3e15 / (1e18 - 3e15) = 3,009.03, up to 3,010
        assert_eq!(
            parts(y, U256::from(3_000_000_000_000_000u64), U256::ZERO),
            Some(U256::from(3_011u64))
        );
        assert_eq!(
            parts(y, U256::ZERO, U256::from(50_000_000_000_000_000u64)),
            Some(U256::from(15_001u64))
        );
        assert_eq!(parts(y, U256::ZERO, U256::ZERO), Some(U256::from(3_001u64)));
        assert_eq!(parts(U256::from(10u64), U256::ZERO, U256::ZERO), Some(U256::from(2u64)));
        assert_eq!(parts(y, U256::from(WAD), U256::ZERO), None);
    }

    /// The guard is the lending venue's where it holds a minimum: sole only where no other live
    /// venue lends the output.
    #[test]
    fn a_direction_is_guarded_where_a_live_venue_holding_a_minimum_lends_its_output() {
        let held = Venue { held_repay: Some(Minimum::Unread), ..venue(3) };
        let venues = |records: Vec<Venue>| Venues { records, unread: false };
        assert_eq!(
            HeldRepay::lending(&venues(vec![held]), 1),
            Some(HeldRepay { minimum: Minimum::Unread, sole: true })
        );
        assert_eq!(HeldRepay::lending(&venues(vec![held]), 0), None);
        assert_eq!(HeldRepay::lending(&venues(vec![venue(1)]), 1), None);
        assert_eq!(
            HeldRepay::lending(&venues(vec![held, venue(1)]), 1),
            Some(HeldRepay { minimum: Minimum::Unread, sole: false })
        );
        assert_eq!(HeldRepay::lending(&venues(vec![Venue { live: false, ..held }]), 1), None);
    }

    #[test]
    fn the_vectors_are_the_shared_bytes() {
        assert_eq!(
            keccak256(VECTORS.as_bytes()),
            b256!("7758eedfa75e7fecaa9d8fc41f194908a2f9eb551c965505e6400affbec50ba8")
        );
    }

    /// Each case's points give a rung's input and output, the pool's own answer at the input and
    /// the label it was priced at.
    #[test]
    fn every_vector_keeps_what_it_says() {
        let vectors: Value = serde_json::from_str(VECTORS).expect("the vectors are JSON");
        let cases = vectors["cases"]
            .as_array()
            .expect("the vectors' cases");
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"]
                .as_str()
                .expect("a case's name");
            let points = case["points"]
                .as_array()
                .expect("a case's points");
            let rungs: Vec<Rung> = points
                .iter()
                .map(|p| Rung {
                    input: number(&p["x"]),
                    output: number(&p["y"]),
                    segment: Segment::Chord,
                })
                .collect();
            let probes: Vec<Probe> = points
                .iter()
                .map(|p| Probe {
                    ask: number(&p["x"]),
                    used: number(&p["x"]),
                    out: number(&p["paid"]),
                    fee_wad: number(&p["label"]),
                    status: ProbeStatus::Priced,
                })
                .collect();
            let legs: Option<Legs> = case["legs"].as_array().map(|words| {
                let mut legs = [U256::ZERO; 8];
                for (leg, word) in legs.iter_mut().zip(words) {
                    *leg = number(word);
                }
                legs
            });
            let guard = HeldRepay {
                minimum: if case["minimum"].is_null() {
                    Minimum::Unread
                } else {
                    Minimum::Read(number(&case["minimum"]))
                },
                sole: case["sole"]
                    .as_bool()
                    .expect("a case's sole"),
            };
            let want = case["keep"]
                .as_array()
                .expect("a case's kept range");
            let index =
                |k: usize| usize::try_from(want[k].as_u64().expect("an index")).expect("an index");
            assert_eq!(
                keep(&rungs, &probes, legs.as_ref(), number(&case["band"]), guard),
                index(0)..index(1),
                "{name}"
            );
        }
    }
}
