//! A ladder of quotes for one direction of one pair: the pool's `previewSwap` answers at
//! increasing inputs, settled by [`settle`] into rungs a swap fills at or above.
//!
//! An Efficient pool's hook prices each size on its curve or, where its row allows leverage, on
//! a leverage route, and labels the answer with that route's fee (`feeWad`). Along one route the
//! output is concave in the input, so a chord between two answers of one route sits at or under
//! the pool. Across a change of route it need not, and a leverage route can stop answering inside
//! its span: near either edge of the hook's price band, and at its floors and token ceilings. So
//! [`settle`] drops leverage answers too close to the band's edges or with no priced answer
//! 1/1024 above them short of where its points stop, cuts a ladder before its leverage answers
//! where the words those tests need are missing or the pool's row fails [`Row::passes`], and
//! quotes a span that changes route at its lower rung, lowered under the ray from the origin
//! through the rung above. A hole in the middle of a leverage route's span with leverage answers
//! well above it is not seen.
//!
//! A ladder moves only its own direction ([`Ladder::after`]); it does not move the book's other
//! side.
use std::ops::Range;

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};

const BPS: u64 = 10_000;

/// An answer's output under this many units is dust: where any answer pays at least this much,
/// answers paying less are not used.
pub const DUST_OUT: u64 = 10_000;

/// A leverage-labelled answer stands only with a priced answer at least `used >> ABOVE_SHIFT`
/// above its input, short of where the ladder's points stop.
pub const ABOVE_SHIFT: usize = 10;

/// An answer's input or output at or past `2^120` is one no pool gives.
pub const ANSWER_BITS: usize = 120;

const WAD: u64 = 1_000_000_000_000_000_000;

/// The least down spread a row may have, 1 bp.
const MIN_SPREAD: u64 = 100_000_000_000_000;

/// How far inside the band each of a row's caps must sit, 0.1 %.
const CAP_MARGIN: u64 = 1_000_000_000_000_000;

/// `a * b / denom` rounded down, `None` for a zero `denom` or a result past `U256`.
fn mul_div(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let product: U512 = a.widening_mul(b);
    U256::checked_from_limbs_slice((product / U512::from(denom)).as_limbs())
}

fn ten_to(exponent: u8) -> U512 {
    U512::from(10u64).pow(U512::from(exponent))
}

/// What one `previewSwap` answer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeStatus {
    /// It uses the input asked, to within 1 bp.
    Priced,
    /// It uses less than the input asked: the pool clipped it.
    Clipped,
    /// The call reverted.
    Refused,
    /// The call ran short of the gas it was given, which says nothing of the pool: a ladder ends
    /// there.
    Starved,
    /// An answer no pool gives: nothing used or paid, more used than asked, or either amount at
    /// or past `2^120`.
    Malformed,
}

/// One `previewSwap` answer, at the input asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub ask: U256,
    pub used: U256,
    pub out: U256,
    /// The label of the route that priced it.
    pub fee_wad: U256,
    pub status: ProbeStatus,
}

impl Probe {
    /// The probe of the pool's `(amountInUsed, amountOut, feeWad)` at `ask`, or of a revert where
    /// `answer` is `None`.
    pub fn classify(ask: U256, answer: Option<(U256, U256, U256)>) -> Self {
        let Some((used, out, fee_wad)) = answer else {
            return Self {
                ask,
                used: U256::ZERO,
                out: U256::ZERO,
                fee_wad: U256::ZERO,
                status: ProbeStatus::Refused,
            };
        };
        let limit = U256::from(1) << ANSWER_BITS;
        let status =
            if used.is_zero() || out.is_zero() || used > ask || used >= limit || out >= limit {
                ProbeStatus::Malformed
            } else if used.widening_mul::<256, 4, 512, 8>(U256::from(BPS)) <
                ask.widening_mul(U256::from(BPS - 1))
            {
                ProbeStatus::Clipped
            } else {
                ProbeStatus::Priced
            };
        Self { ask, used, out, fee_wad, status }
    }

    fn live(&self) -> bool {
        matches!(self.status, ProbeStatus::Priced | ProbeStatus::Clipped)
    }

    /// Priced or clipped, and paying at least [`DUST_OUT`].
    pub fn qualifies(&self) -> bool {
        self.live() && self.out >= U256::from(DUST_OUT)
    }
}

/// Which way a direction trades the hook pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Token 0, the pool asset, in.
    Sale,
    /// Token 1 in, the pool asset out.
    Buy,
}

/// How the spans of a pool's ladders are quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A hook of a listed class: chords along a route, the whole rule across routes.
    Everlong,
    /// A hook with no leverage row in a pool named by whoever configured the feed, which vouches
    /// that its output never falls with size: every span is quoted at its lower rung.
    Step,
}

/// Whether leverage answers may stand on a pool's ladders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The row passes [`Row::passes`] at the live band and the pool has exactly two tokens.
    Trusted,
    /// Every ladder is cut before its first leverage-labelled point.
    Untrusted,
}

/// The words of a hook's leverage row in its `params()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub spread_wad: U256,
    pub spread_dn_wad: U256,
    pub up_cap_wad: U256,
    pub dn_cap_wad: U256,
    pub band_wad: U256,
}

impl Row {
    /// Whether the row allows leverage: a zero `spreadWad` is no row.
    pub fn has_leverage(&self) -> bool {
        !self.spread_wad.is_zero()
    }

    /// The labels of the row's leverage routes: none without leverage, one where both spreads
    /// are equal.
    pub fn tiers(&self) -> Tiers {
        if !self.has_leverage() {
            return Tiers::default();
        }
        let mut labels = vec![self.spread_wad];
        if self.spread_dn_wad != self.spread_wad {
            labels.push(self.spread_dn_wad);
        }
        Tiers(labels)
    }

    /// Whether leverage answers may stand at `live_band`, the pool's band for the pair: both caps
    /// set and each 0.1 % inside the narrower band, and a down spread of at least 1 bp. A row
    /// with no leverage never passes.
    pub fn passes(&self, live_band: U256) -> bool {
        let band = self.band_wad.min(live_band);
        if band.is_zero() ||
            self.dn_cap_wad.is_zero() ||
            self.up_cap_wad.is_zero() ||
            self.spread_dn_wad < U256::from(MIN_SPREAD)
        {
            return false;
        }
        [self.dn_cap_wad, self.up_cap_wad]
            .into_iter()
            .all(|cap| {
                let Some(wide) = U256::from(WAD).checked_add(band) else { return false };
                mul_div(cap, wide, U256::from(WAD))
                    .and_then(|reach| reach.checked_add(U256::from(CAP_MARGIN)))
                    .is_some_and(|reach| reach <= band)
            })
    }
}

impl Trust {
    /// Trusted where `row` passes at `live_band` and the pool has exactly two tokens.
    pub fn of(row: &Row, live_band: U256, tokens: usize) -> Self {
        if tokens == 2 && row.passes(live_band) {
            Self::Trusted
        } else {
            Self::Untrusted
        }
    }
}

/// The labels of a hook's leverage routes, from its row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tiers(Vec<U256>);

impl Tiers {
    pub fn contains(&self, label: U256) -> bool {
        self.0.contains(&label)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The words of one direction's `pairRequest` the rule reads, and on a buy word 6 of the hook's
/// `levState`; all zero where they were not read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub band_wad: U256,
    pub usd_in: U256,
    pub usd_out: U256,
    /// The input's held, supplied, posted and owed legs, then the output's.
    pub legs: [U256; 8],
    /// A buy's lever-up top bid; a sale does not read it.
    pub bid_wad: U256,
}

/// What [`settle`] knows of a direction besides its probes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub rule: Rule,
    pub tiers: Tiers,
    /// The row's band, `params()` word 32.
    pub row_band: U256,
    pub side: Side,
    pub decimals_in: u8,
    pub decimals_out: u8,
    pub trust: Trust,
    /// The curve's fee at the request, `quotePair`'s `curveFeeWad`; zero where not read.
    pub curve_fee: U256,
    pub request: Request,
}

/// How a ladder quotes the span that ends at a rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Segment {
    /// On the chord from the rung below, rounded down. The first rung's, which ends no span.
    Chord,
    /// At the rung below's output: the span changes route, or its hook is quoted by steps.
    Flat,
}

/// One quoted input: `input` fills in full for at least `output`, net of every fee and of the
/// feed's haircut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rung {
    pub input: U256,
    pub output: U256,
    pub segment: Segment,
}

/// Rungs in strictly increasing input and non-decreasing output, the first output above zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ladder(Vec<Rung>);

/// What [`settle`] makes of a direction's probes, on the pool's own outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub ladder: Ladder,
    /// The top is a curve-labelled clip: the pool fills at most its input, whatever is asked.
    /// Quotes stop at the top either way; the shared vectors pin it.
    pub capped: bool,
}

/// The words of the room test, in output units.
struct Room {
    side: Side,
    decimals_in: u8,
    decimals_out: u8,
    /// Token 0's price in token 1 units per unit, scaled by `1e36`.
    p36: U512,
    band: U512,
    need: U512,
    held_out: U512,
    idle_in: U512,
}

impl Room {
    /// The room test's words, or `None` where the request was not read or reads no price, or a
    /// buy reads no pool asset to sell.
    fn read(terms: &Terms) -> Option<Self> {
        let request = &terms.request;
        // the sale's margin divides by 10^(36 - decimals_out)
        if terms.decimals_in > 36 || terms.decimals_out > 36 {
            return None;
        }
        let (usd_0, usd_1, decimals_0, decimals_1) = match terms.side {
            Side::Sale => (request.usd_in, request.usd_out, terms.decimals_in, terms.decimals_out),
            Side::Buy => (request.usd_out, request.usd_in, terms.decimals_out, terms.decimals_in),
        };
        let e36 = ten_to(36);
        let denom = U512::from(usd_1) * ten_to(decimals_0);
        let p36 = if denom.is_zero() {
            U512::ZERO
        } else {
            U512::from(usd_0) * ten_to(decimals_1) * e36 / denom
        };
        let [held_in, supplied_in, _, owed_in, held_out, supplied_out, posted_out, _] =
            request.legs;
        let held_out = U512::from(held_out) + U512::from(supplied_out) + U512::from(posted_out);
        if request.band_wad.is_zero() ||
            request.usd_in.is_zero() ||
            request.usd_out.is_zero() ||
            p36.is_zero() ||
            (terms.side == Side::Buy && held_out.is_zero())
        {
            return None;
        }
        let unit = match terms.side {
            Side::Sale => p36 / e36 + U512::from(1),
            Side::Buy => U512::from(1),
        };
        let idle =
            (U512::from(held_in) + U512::from(supplied_in)).saturating_sub(U512::from(owed_in));
        Some(Self {
            side: terms.side,
            decimals_in: terms.decimals_in,
            decimals_out: terms.decimals_out,
            p36,
            band: U512::from(terms.row_band.min(request.band_wad)),
            need: U512::from(2) + U512::from(3) * unit,
            held_out,
            idle_in: idle * e36 / p36,
        })
    }

    /// Whether an answer of `used` for `out` sits at least the margin `test` asks inside both
    /// edges of the band. An answer whose arithmetic overflows does not.
    fn passes(&self, used: U256, out: U256, test: RoomTest) -> bool {
        self.check(U512::from(used), U512::from(out), test)
            .unwrap_or(false)
    }

    fn check(&self, used: U512, out: U512, test: RoomTest) -> Option<bool> {
        let (e36, wad) = (ten_to(36), U512::from(WAD));
        // a band at or past 100 % leaves no room under the price
        let low = wad.saturating_sub(self.band);
        let high = wad.checked_add(self.band)?;
        let (min_out, max_out, margin) = match self.side {
            Side::Sale => {
                let value = used.checked_mul(self.p36)?;
                let scale = e36.checked_mul(wad)?;
                let min_out = value.checked_mul(low)? / scale + U512::from(1);
                let max_out = value.checked_mul(high)? / scale;
                let grid = used / ten_to(36 - self.decimals_out);
                (min_out, max_out, self.need.checked_add(grid)? + U512::from(1))
            }
            Side::Buy => {
                let value = used.checked_mul(e36)?;
                let scale = self.p36.checked_mul(wad)?;
                let min_out = value.checked_mul(low)? / scale + U512::from(1);
                let max_out = value.checked_mul(high)? / scale;
                let noise = (self.held_out + self.idle_in) / ten_to(16);
                let grid = out.checked_mul(ten_to(self.decimals_in))? / self.p36;
                (
                    min_out,
                    max_out,
                    self.need
                        .checked_add(noise)?
                        .checked_add(grid)? +
                        U512::from(1),
                )
            }
        };
        let margin = match test {
            RoomTest::Margin => margin,
            #[cfg(test)]
            RoomTest::Need => self.need,
            #[cfg(test)]
            RoomTest::Off => return Some(true),
        };
        Some(out >= min_out.checked_add(margin)? && out.checked_add(margin)? <= max_out)
    }
}

/// How much room [`settle_with`] asks of a tier-labelled answer inside the band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RoomTest {
    /// The whole margin: the rule.
    Margin,
    /// Only the margin's fixed part, without the terms for rounding and noise that grow with
    /// the fill.
    #[cfg(test)]
    Need,
    /// None.
    #[cfg(test)]
    Off,
}

/// Whether [`settle_with`] asks for a priced answer `used >> ABOVE_SHIFT` above each
/// tier-labelled one, short of where the points stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AboveTest {
    /// The rule.
    Priced,
    #[cfg(test)]
    Off,
}

#[derive(Clone, Copy)]
struct Point {
    x: U256,
    y: U256,
    label: U256,
}

/// The points one run over a direction's answers makes.
struct Run {
    points: Vec<Point>,
    /// The pool fills at most the last point's input, on the curve's label.
    capped: bool,
    /// The largest input of a priced answer among the points; zero where there is none.
    top: U256,
}

impl Run {
    /// The points `probes`, ascending by ask, make: their answers in order, up to the first that
    /// starves or is malformed, refuses after a point paying at least [`DUST_OUT`], uses no more
    /// than the last point, falls other than off a `tiers` label, or carries a third label. A
    /// clip is the last point. A refusal before any point, or after one paying less than
    /// [`DUST_OUT`], clears them.
    fn of(probes: &[Probe], tiers: &Tiers) -> Self {
        let dust_out = U256::from(DUST_OUT);
        let mut points: Vec<Point> = Vec::with_capacity(probes.len());
        let mut capped = false;
        let mut top = U256::ZERO;
        for p in probes {
            let last = points.last().copied();
            if p.status == ProbeStatus::Refused && last.is_none_or(|l| l.y < dust_out) {
                points.clear();
                capped = false;
                top = U256::ZERO;
                continue;
            }
            if !p.live() {
                break;
            }
            if let Some(l) = last {
                if p.used <= l.x {
                    capped = p.used == l.x && p.fee_wad == l.label && !tiers.contains(l.label);
                    break;
                }
                if p.out < l.y && !(tiers.contains(l.label) && p.fee_wad != l.label) {
                    break;
                }
                let first = points[0].label;
                let second = points
                    .iter()
                    .map(|q| q.label)
                    .find(|label| *label != first);
                if p.fee_wad != first && second.is_some_and(|second| second != p.fee_wad) {
                    break;
                }
            }
            points.push(Point { x: p.used, y: p.out, label: p.fee_wad });
            if p.status == ProbeStatus::Priced {
                top = top.max(p.used);
            }
            if p.status == ProbeStatus::Clipped {
                capped = !tiers.contains(p.fee_wad);
                break;
            }
        }
        Self { points, capped, top }
    }
}

/// The rungs `probes`, ascending by ask, support under `terms`, on the pool's own outputs.
///
/// Dust goes first. On a pool with leverage tiers under [`Rule::Everlong`], a tier-labelled
/// answer then goes where it lacks the room test's margin inside the band, or where no priced
/// answer sits `used >> ABOVE_SHIFT` above it among the points the answers left make: one past
/// where those points stop backs nothing. Points then run over the answers that stay, from the
/// first up to the first refusal past one that pays at least [`DUST_OUT`], the first answer that
/// does not extend them, a fall other than off a tier label, a third label or a clip. A ladder
/// with no tiers is cut at its first change of label; one with tiers is cut before its first
/// tier-labelled point where the pool is not trusted, the request was not read, a buy's bid reads
/// zero, or its labels run tier, other, that tier. Spans that change route, every span under
/// [`Rule::Step`], and on a ladder of one tier label every span unless the curve's fee reads
/// apart from it, are flat. From the top down each point is lowered to the one above it, and
/// where a flat span starts on a tier label, under the ray from the origin through the point
/// above. Points lowered to zero at the low end go.
pub fn settle(probes: &[Probe], terms: &Terms) -> Settled {
    settle_with(probes, terms, RoomTest::Margin, AboveTest::Priced)
}

/// [`settle`] with its two tests on tier-labelled answers as given: the rule is
/// [`RoomTest::Margin`] and [`AboveTest::Priced`]. The others leave a test out, to show what it
/// keeps off a ladder.
pub(super) fn settle_with(
    probes: &[Probe],
    terms: &Terms,
    room_test: RoomTest,
    above_test: AboveTest,
) -> Settled {
    let tiers = &terms.tiers;
    let tiered = terms.rule == Rule::Everlong && !tiers.is_empty();
    let dust_out = U256::from(DUST_OUT);
    let mut left = probes.to_vec();
    if left.iter().any(Probe::qualifies) {
        left.retain(|p| !(p.live() && p.out < dust_out));
    }
    let mut unread = false;
    if tiered {
        match Room::read(terms) {
            Some(room) => left.retain(|p| {
                !(p.live() && tiers.contains(p.fee_wad)) || room.passes(p.used, p.out, room_test)
            }),
            None => unread = true,
        }
        if above_test == AboveTest::Priced {
            // a priced answer past where the points stop is no point, so no ray from it lowers
            // the leverage answers under it: it backs nothing
            let top = Run::of(&left, tiers).top;
            left.retain(|p| {
                !(p.live() && tiers.contains(p.fee_wad)) ||
                    top >= p
                        .used
                        .saturating_add(p.used >> ABOVE_SHIFT)
            });
        }
    }

    let Run { mut points, mut capped, .. } = Run::of(&left, tiers);
    let before_cuts = points.len();
    if terms.rule == Rule::Everlong && !points.is_empty() {
        let first = points[0].label;
        let cut = if tiers.is_empty() {
            points
                .iter()
                .position(|q| q.label != first)
        } else {
            let mut runs: Vec<U256> = Vec::with_capacity(points.len());
            for q in &points {
                if runs.last() != Some(&q.label) {
                    runs.push(q.label);
                }
            }
            let returns_to_a_tier = runs
                .windows(3)
                .any(|w| w[0] == w[2] && tiers.contains(w[0]));
            let no_bid = terms.side == Side::Buy && terms.request.bid_wad.is_zero();
            if terms.trust == Trust::Untrusted || unread || no_bid || returns_to_a_tier {
                points
                    .iter()
                    .position(|q| tiers.contains(q.label))
            } else {
                None
            }
        };
        if let Some(k) = cut {
            points.truncate(k);
        }
    }
    if points.len() != before_cuts {
        capped = false;
    }

    let one_label = points
        .iter()
        .all(|q| q.label == points[0].label);
    let mut flat = vec![false; points.len()];
    for k in 1..points.len() {
        flat[k] = match terms.rule {
            Rule::Step => true,
            Rule::Everlong if tiers.is_empty() => false,
            Rule::Everlong if one_label => {
                let label = points[0].label;
                tiers.contains(label) && (terms.curve_fee.is_zero() || terms.curve_fee == label)
            }
            Rule::Everlong => points[k - 1].label != points[k].label,
        };
    }
    for k in (1..points.len()).rev() {
        let above = points[k];
        let below = &mut points[k - 1];
        if terms.rule == Rule::Everlong && flat[k] && tiers.contains(below.label) {
            // x[k] > x[k - 1], so the ray at x[k - 1] is at most y[k] and always fits
            let ray = mul_div(above.y, below.x, above.x).unwrap_or_default();
            below.y = below.y.min(ray);
        }
        below.y = below.y.min(above.y);
    }

    let start = points
        .iter()
        .position(|q| !q.y.is_zero())
        .unwrap_or(points.len());
    let rungs = points
        .iter()
        .zip(&flat)
        .enumerate()
        .skip(start)
        .map(|(k, (q, flat))| Rung {
            input: q.x,
            output: q.y,
            segment: if *flat && k > start { Segment::Flat } else { Segment::Chord },
        })
        .collect();
    Settled { ladder: Ladder(rungs), capped }
}

impl Ladder {
    /// The ladder [`settle`] makes of `probes`, each output then keeping `10_000 - haircut_bps`
    /// of itself, rounded down. Rounding down keeps the outputs' order; a rung it takes to zero
    /// goes, and the rung that then comes first ends no span.
    pub fn settle(probes: &[Probe], terms: &Terms, haircut_bps: u64) -> Self {
        settle(probes, terms)
            .ladder
            .haircut(haircut_bps)
    }

    /// The ladder with each output keeping `10_000 - haircut_bps` of itself, rounded down, as
    /// [`Ladder::settle`] leaves it.
    pub(super) fn haircut(self, haircut_bps: u64) -> Self {
        let keep = U256::from(BPS.saturating_sub(haircut_bps));
        let mut rungs: Vec<Rung> = Vec::with_capacity(self.0.len());
        for rung in self.0 {
            // at most the output itself, so it always fits
            let output = mul_div(rung.output, keep, U256::from(BPS)).unwrap_or_default();
            if output.is_zero() && rungs.is_empty() {
                continue;
            }
            let segment = if rungs.is_empty() { Segment::Chord } else { rung.segment };
            rungs.push(Rung { output, segment, ..rung });
        }
        Self(rungs)
    }

    #[cfg(test)]
    pub fn from_rungs(rungs: Vec<Rung>) -> Self {
        Self(rungs)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn rungs(&self) -> &[Rung] {
        &self.0
    }

    /// The rungs in `kept` alone, the first of them ending no span: nothing is quoted under it.
    pub(super) fn keep(self, kept: Range<usize>) -> Self {
        let start = kept.start;
        Self(
            self.0
                .into_iter()
                .enumerate()
                .filter(|(k, _)| kept.contains(k))
                .map(
                    |(k, rung)| {
                        if k == start {
                            Rung { segment: Segment::Chord, ..rung }
                        } else {
                            rung
                        }
                    },
                )
                .collect(),
        )
    }

    /// The smallest quoted fill.
    pub fn first(&self) -> Option<Rung> {
        self.0.first().copied()
    }

    /// The largest quoted fill, the direction's limit.
    pub fn last(&self) -> Option<Rung> {
        self.0.last().copied()
    }

    /// The output of `amount_in`: a rung's own at its input, the rung below's inside a flat span,
    /// and inside any other the chord's, rounded down. `None` under the first rung's input and
    /// past the last's.
    pub fn output(&self, amount_in: U256) -> Option<U256> {
        let first = self.0.first()?;
        if amount_in < first.input {
            return None;
        }
        let mut below = *first;
        for rung in &self.0 {
            if amount_in == rung.input {
                return Some(rung.output);
            }
            if amount_in < rung.input {
                return match rung.segment {
                    Segment::Flat => Some(below.output),
                    Segment::Chord => {
                        let rise = rung.output.checked_sub(below.output)?;
                        let run = rung.input.checked_sub(below.input)?;
                        let along = mul_div(rise, amount_in.checked_sub(below.input)?, run)?;
                        below.output.checked_add(along)
                    }
                };
            }
            below = *rung;
        }
        None
    }

    /// The ladder left once `amount_in` went through it. Empty where any span is flat: a lowered
    /// rung no longer says what the pool paid, so it bounds nothing that is left. Otherwise the
    /// pool paid at most the output of the first rung strictly past `amount_in` (a fill that lands
    /// on a rung leaves no margin, so the next rung bounds it), so each later rung
    /// keeps its output less that bound: the shifted ladder never quotes more than the pool has
    /// left on this side.
    pub fn after(&self, amount_in: U256) -> Self {
        if self
            .0
            .iter()
            .any(|rung| rung.segment == Segment::Flat)
        {
            return Self::default();
        }
        let Some(bound) = self
            .0
            .iter()
            .find(|rung| rung.input > amount_in)
            .map(|rung| rung.output)
        else {
            return Self::default();
        };
        Self(
            self.0
                .iter()
                .filter(|rung| rung.input > amount_in && rung.output > bound)
                .map(|rung| Rung {
                    input: rung.input - amount_in,
                    output: rung.output - bound,
                    segment: Segment::Chord,
                })
                .collect(),
        )
    }
}

/// The inputs a ladder is quoted at: `count` sizes doubling from `smallest`, never past
/// `largest` (the token's `maxSwapNotional` where it sets one), which closes the ladder.
pub fn rung_inputs(smallest: U256, largest: Option<U256>, count: usize) -> Vec<U256> {
    let mut inputs = Vec::with_capacity(count);
    let mut size = smallest.max(U256::from(1));
    for _ in 0..count {
        match largest {
            Some(cap) if size >= cap => {
                inputs.push(cap);
                break;
            }
            _ => inputs.push(size),
        }
        size = size.saturating_mul(U256::from(2));
    }
    inputs
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{b256, keccak256};
    use rstest::rstest;
    use serde_json::Value;

    use super::*;

    /// The rule's shared vectors, the same bytes in every client that settles these pools.
    const VECTORS: &str = include_str!("testdata/settle_vectors.json");

    fn u(v: u64) -> U256 {
        U256::from(v)
    }

    fn word(value: &Value) -> U256 {
        value
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("not a decimal word: {value}"))
    }

    fn words(value: &Value) -> Vec<U256> {
        value
            .as_array()
            .unwrap_or_else(|| panic!("not a list: {value}"))
            .iter()
            .map(word)
            .collect()
    }

    fn decimals(value: &Value) -> u8 {
        value
            .as_u64()
            .and_then(|d| u8::try_from(d).ok())
            .unwrap_or_else(|| panic!("not decimals: {value}"))
    }

    fn status(value: &Value) -> ProbeStatus {
        match value.as_u64() {
            Some(1) => ProbeStatus::Priced,
            Some(2) => ProbeStatus::Clipped,
            Some(3) => ProbeStatus::Refused,
            Some(4) => ProbeStatus::Starved,
            Some(5) => ProbeStatus::Malformed,
            other => panic!("no probe status {other:?}"),
        }
    }

    fn vector_terms(case: &Value) -> Terms {
        let r = &case["request"];
        let request = if r.is_null() {
            Request::default()
        } else {
            let legs: [U256; 8] = words(&r["legs"])
                .try_into()
                .unwrap_or_else(|legs| panic!("eight legs, not {legs:?}"));
            Request {
                band_wad: word(&r["bandWad"]),
                usd_in: word(&r["usdIn"]),
                usd_out: word(&r["usdOut"]),
                legs,
                bid_wad: word(&r["bidWad"]),
            }
        };
        Terms {
            rule: match case["rule"].as_str() {
                Some("EVERLONG") => Rule::Everlong,
                Some("STEP") => Rule::Step,
                other => panic!("no rule {other:?}"),
            },
            tiers: Tiers(words(&case["tiers"])),
            row_band: word(&case["rowBand"]),
            side: if case["sale"].as_bool() == Some(true) { Side::Sale } else { Side::Buy },
            decimals_in: decimals(&case["decIn"]),
            decimals_out: decimals(&case["decOut"]),
            trust: if case["trusted"].as_bool() == Some(true) {
                Trust::Trusted
            } else {
                Trust::Untrusted
            },
            curve_fee: word(&case["curveFee"]),
            request,
        }
    }

    fn vector_probes(case: &Value) -> Vec<Probe> {
        case["probes"]
            .as_array()
            .unwrap_or_else(|| panic!("no probes in {case}"))
            .iter()
            .map(|p| Probe {
                ask: word(&p[0]),
                used: word(&p[1]),
                out: word(&p[2]),
                fee_wad: word(&p[3]),
                status: status(&p[4]),
            })
            .collect()
    }

    /// `x`, `y`, `flat` and `capped`, as the vectors write a ladder.
    fn shape(settled: &Settled) -> (Vec<U256>, Vec<U256>, Vec<bool>, bool) {
        let rungs = settled.ladder.rungs();
        (
            rungs.iter().map(|r| r.input).collect(),
            rungs.iter().map(|r| r.output).collect(),
            rungs
                .iter()
                .map(|r| r.segment == Segment::Flat)
                .collect(),
            settled.capped,
        )
    }

    fn vectors() -> Value {
        serde_json::from_str(VECTORS).expect("the vectors are JSON")
    }

    #[test]
    fn the_vectors_are_the_shared_bytes() {
        assert_eq!(
            keccak256(VECTORS.as_bytes()),
            b256!("c6e03787929da4f882b3cefb7f892989aeaf6e7783dec5fb7933d6fe2331fde2")
        );
    }

    #[test]
    fn every_vector_settles_as_written() {
        let vectors = vectors();
        let cases = vectors["cases"]
            .as_array()
            .expect("cases");
        assert_eq!(cases.len(), 76);
        let mut wrong = Vec::new();
        for case in cases {
            let want = &case["want"];
            let want = (
                words(&want["x"]),
                words(&want["y"]),
                want["flat"]
                    .as_array()
                    .expect("flat")
                    .iter()
                    .map(|f| f.as_bool().expect("a flag"))
                    .collect::<Vec<_>>(),
                want["capped"]
                    .as_bool()
                    .expect("capped"),
            );
            let got = shape(&settle(&vector_probes(case), &vector_terms(case)));
            if got != want {
                wrong.push(format!("{}: got {got:?}, want {want:?}", case["name"]));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} vectors:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn every_row_vector_passes_or_fails_as_written() {
        let vectors = vectors();
        let rows = vectors["rowOk"]
            .as_array()
            .expect("rowOk");
        assert_eq!(rows.len(), 13);
        for row in rows {
            // the check reads no up spread, and the vectors give none
            let verdict = Row {
                spread_dn_wad: word(&row["spreadDn"]),
                up_cap_wad: word(&row["upCap"]),
                dn_cap_wad: word(&row["dnCap"]),
                band_wad: word(&row["rowBand"]),
                ..Row::default()
            }
            .passes(word(&row["live"]));
            assert_eq!(Some(verdict), row["want"].as_bool(), "{}", row["name"]);
        }
    }

    const T1: u64 = 17_500_000_000_000_000;
    const T2: u64 = 13_000_000_000_000_000;
    const CURVE: u64 = 15_494_000_000_000_000;
    const OTHER: u64 = 16_000_000_000_000_000;
    const M: u64 = 1_000_000;

    fn priced(ask: u64, out: u64, label: u64) -> Probe {
        Probe {
            ask: u(ask),
            used: u(ask),
            out: u(out),
            fee_wad: u(label),
            status: ProbeStatus::Priced,
        }
    }

    fn clipped(ask: u64, used: u64, out: u64, label: u64) -> Probe {
        Probe {
            ask: u(ask),
            used: u(used),
            out: u(out),
            fee_wad: u(label),
            status: ProbeStatus::Clipped,
        }
    }

    fn refused(ask: u64) -> Probe {
        Probe::classify(u(ask), None)
    }

    /// An 8/6 pair at 105,000 USD per token 0, a row of band 5 % and caps .5 % and 2 % at a live
    /// band of 8 %, on a two-token pool.
    fn terms(side: Side) -> Terms {
        let row = Row {
            spread_wad: u(T1),
            spread_dn_wad: u(T2),
            up_cap_wad: u(5_000_000_000_000_000),
            dn_cap_wad: u(20_000_000_000_000_000),
            band_wad: u(50_000_000_000_000_000),
        };
        let live = u(80_000_000_000_000_000);
        let (usd_in, usd_out, decimals_in, decimals_out, legs, bid_wad) = match side {
            Side::Sale => {
                (u(105_000) * U256::from(WAD), U256::from(WAD), 8, 6, [U256::ZERO; 8], U256::ZERO)
            }
            Side::Buy => {
                let mut legs = [U256::ZERO; 8];
                legs[4] = u(100_000_000);
                (U256::from(WAD), u(105_000) * U256::from(WAD), 6, 8, legs, U256::from(WAD))
            }
        };
        Terms {
            rule: Rule::Everlong,
            tiers: row.tiers(),
            row_band: row.band_wad,
            side,
            decimals_in,
            decimals_out,
            trust: Trust::of(&row, live, 2),
            curve_fee: U256::ZERO,
            request: Request { band_wad: live, usd_in, usd_out, legs, bid_wad },
        }
    }

    fn sale() -> Terms {
        terms(Side::Sale)
    }

    struct Want {
        x: Vec<u64>,
        y: Vec<u64>,
        flat: Vec<bool>,
        capped: bool,
    }

    fn want(x: &[u64], y: &[u64], flat: &[bool], capped: bool) -> Want {
        Want { x: x.to_vec(), y: y.to_vec(), flat: flat.to_vec(), capped }
    }

    /// Cases beyond the shared vectors, each the rule's answer for the probes and terms it names.
    #[rstest]
    #[case::leverage_then_curve(
        sale(),
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_079_000_000, T1), priced(4 * M, 4_100_000_000, CURVE), priced(8 * M, 8_150_000_000, CURVE)],
        want(&[M, 2 * M, 4 * M, 8 * M], &[1_040_000_000, 2_050_000_000, 4_100_000_000, 8_150_000_000], &[false, false, true, false], false),
    )]
    #[case::curve_then_leverage_nothing_above_the_top(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_200_000_000, T1), priced(8 * M, 8_380_000_000, T1), priced(16 * M, 16_700_000_000, T1)],
        want(&[M, 2 * M, 4 * M, 8 * M], &[1_033_000_000, 2_066_000_000, 4_200_000_000, 8_380_000_000], &[false, false, true, false], false),
    )]
    #[case::a_drop_to_the_curve_with_no_visible_fall(
        sale(),
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_080_000_000, T1), priced(4 * M, 4_050_000_000, CURVE), priced(8 * M, 8_100_000_000, CURVE)],
        want(&[M, 2 * M, 4 * M, 8 * M], &[1_040_000_000, 2_025_000_000, 4_050_000_000, 8_100_000_000], &[false, false, true, false], false),
    )]
    #[case::a_drop_to_the_curve_with_a_fall(
        sale(),
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_080_000_000, T1), priced(2_100_000, 2_070_000_000, CURVE), priced(8 * M, 8_100_000_000, CURVE)],
        want(&[M, 2 * M, 2_100_000, 8 * M], &[1_040_000_000, 1_971_428_571, 2_070_000_000, 8_100_000_000], &[false, false, true, false], false),
    )]
    #[case::equal_spreads_give_one_tier_label(
        Terms { tiers: Row { spread_wad: u(T1), spread_dn_wad: u(T1), ..Row::default() }.tiers(), ..sale() },
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_200_000_000, T1), priced(8 * M, 8_380_000_000, CURVE)],
        want(&[M, 2 * M, 4 * M, 8 * M], &[1_033_000_000, 2_066_000_000, 4_190_000_000, 8_380_000_000], &[false, false, true, true], false),
    )]
    #[case::one_tier_label_the_curve_fee_read(
        Terms { curve_fee: u(CURVE), ..sale() },
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_079_000_000, T1), priced(4 * M, 4_150_000_000, T1), priced(8 * M, 8_290_000_000, T1)],
        want(&[M, 2 * M, 4 * M], &[1_040_000_000, 2_079_000_000, 4_150_000_000], &[false, false, false], false),
    )]
    #[case::one_tier_label_the_curve_fee_tied(
        Terms { curve_fee: u(T1), ..sale() },
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_079_000_000, T1), priced(4 * M, 4_150_000_000, T1), priced(8 * M, 8_290_000_000, T1)],
        want(&[M, 2 * M, 4 * M], &[1_037_500_000, 2_075_000_000, 4_150_000_000], &[false, true, true], false),
    )]
    #[case::one_tier_label_the_curve_fee_unread(
        sale(),
        vec![priced(M, 1_040_000_000, T1), priced(2 * M, 2_079_000_000, T1), priced(4 * M, 4_150_000_000, T1), priced(8 * M, 8_290_000_000, T1)],
        want(&[M, 2 * M, 4 * M], &[1_037_500_000, 2_075_000_000, 4_150_000_000], &[false, true, true], false),
    )]
    #[case::no_row(
        Terms { tiers: Row::default().tiers(), request: Request::default(), ..sale() },
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_200_000_000, OTHER), priced(8 * M, 8_380_000_000, OTHER)],
        want(&[M, 2 * M], &[1_033_000_000, 2_066_000_000], &[false, false], false),
    )]
    #[case::three_tokens(
        Terms { trust: Trust::of(&Row { spread_wad: u(T1), spread_dn_wad: u(T2), up_cap_wad: u(5_000_000_000_000_000), dn_cap_wad: u(20_000_000_000_000_000), band_wad: u(50_000_000_000_000_000) }, u(80_000_000_000_000_000), 3), ..sale() },
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_200_000_000, T1), priced(8 * M, 8_380_000_000, T1), priced(16 * M, 16_500_000_000, CURVE)],
        want(&[M, 2 * M], &[1_033_000_000, 2_066_000_000], &[false, false], false),
    )]
    #[case::row_fails(
        Terms { trust: Trust::of(&Row { spread_wad: u(T1), spread_dn_wad: u(T2), up_cap_wad: u(5_000_000_000_000_000), dn_cap_wad: U256::ZERO, band_wad: u(50_000_000_000_000_000) }, u(80_000_000_000_000_000), 2), ..sale() },
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_200_000_000, T1), priced(8 * M, 8_380_000_000, T1), priced(16 * M, 16_500_000_000, CURVE)],
        want(&[M, 2 * M], &[1_033_000_000, 2_066_000_000], &[false, false], false),
    )]
    #[case::tier_curve_tier(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_090_000_000, T1), priced(4 * M, 4_133_000_000, CURVE), priced(8 * M, 8_380_000_000, T1), priced(16 * M, 16_700_000_000, T1)],
        want(&[M], &[1_033_000_000], &[false], false),
    )]
    #[case::curve_tier_curve(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_090_000_000, T1), priced(4 * M, 4_133_000_000, CURVE), priced(8 * M, 8_250_000_000, CURVE)],
        want(&[M, 2 * M, 4 * M, 8 * M], &[1_033_000_000, 2_066_500_000, 4_133_000_000, 8_250_000_000], &[false, true, true, false], false),
    )]
    #[case::a_tier_point_under_a_third_label_goes_and_the_points_run_past_it(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_090_000_000, T1), priced(4 * M, 4_150_000_000, OTHER), priced(8 * M, 8_300_000_000, OTHER)],
        want(&[M, 4 * M, 8 * M], &[1_033_000_000, 4_150_000_000, 8_300_000_000], &[false, true, false], false),
    )]
    #[case::a_curve_clip_at_the_top_is_kept_capped(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), clipped(4 * M, 3 * M, 3_099_000_000, CURVE)],
        want(&[M, 2 * M, 3 * M], &[1_033_000_000, 2_066_000_000, 3_099_000_000], &[false, false, false], true),
    )]
    #[case::a_tier_clip_at_the_top_goes(
        sale(),
        vec![priced(M, 1_033_000_000, CURVE), priced(2 * M, 2_066_000_000, CURVE), clipped(4 * M, 3 * M, 3_140_000_000, T1)],
        want(&[M, 2 * M], &[1_033_000_000, 2_066_000_000], &[false, false], false),
    )]
    #[case::a_leading_refusal(
        sale(),
        vec![refused(M), priced(2 * M, 2_066_000_000, CURVE), priced(4 * M, 4_130_000_000, CURVE)],
        want(&[2 * M, 4 * M], &[2_066_000_000, 4_130_000_000], &[false, false], false),
    )]
    #[case::the_step_rule(
        Terms { rule: Rule::Step, tiers: Tiers::default(), request: Request::default(), ..sale() },
        vec![priced(M, 1_000_000_000, CURVE), priced(2 * M, 2_100_000_000, CURVE), priced(4 * M, 3_900_000_000, OTHER)],
        want(&[M, 2 * M, 4 * M], &[1_000_000_000, 2_100_000_000, 3_900_000_000], &[false, true, true], false),
    )]
    #[case::a_buy_with_a_zero_bid_is_cut_before_its_tier_points(
        Terms { request: Request { bid_wad: U256::ZERO, ..terms(Side::Buy).request }, ..terms(Side::Buy) },
        vec![priced(20 * M, 18_900, CURVE), priced(40 * M, 37_900, CURVE), priced(80 * M, 76_000, T2), priced(160 * M, 152_100, T2), priced(320 * M, 304_500, CURVE)],
        want(&[20 * M, 40 * M], &[18_900, 37_900], &[false, false], false),
    )]
    #[case::a_buy_with_a_bid_keeps_its_tier_points(
        terms(Side::Buy),
        vec![priced(20 * M, 18_900, CURVE), priced(40 * M, 37_900, CURVE), priced(80 * M, 76_000, T2), priced(160 * M, 152_100, T2), priced(320 * M, 304_500, CURVE)],
        want(&[20 * M, 40 * M, 80 * M, 160 * M, 320 * M], &[18_900, 37_900, 76_000, 152_100, 304_500], &[false, false, true, false, true], false),
    )]
    fn the_rule_settles(#[case] terms: Terms, #[case] probes: Vec<Probe>, #[case] want: Want) {
        let got = shape(&settle(&probes, &terms));
        let want = (
            want.x
                .into_iter()
                .map(u)
                .collect::<Vec<_>>(),
            want.y
                .into_iter()
                .map(u)
                .collect::<Vec<_>>(),
            want.flat,
            want.capped,
        );
        assert_eq!(got, want);
    }

    #[rstest]
    #[case::priced(u(100), Some((u(100), u(5), u(1))), ProbeStatus::Priced)]
    #[case::one_bp_short_is_priced(u(10_000), Some((u(9_999), u(5), u(1))), ProbeStatus::Priced)]
    #[case::past_one_bp_short_is_clipped(u(10_000), Some((u(9_998), u(5), u(1))), ProbeStatus::Clipped)]
    #[case::a_revert_is_refused(u(100), None, ProbeStatus::Refused)]
    #[case::nothing_used(u(100), Some((U256::ZERO, u(5), u(1))), ProbeStatus::Malformed)]
    #[case::nothing_paid(u(100), Some((u(100), U256::ZERO, u(1))), ProbeStatus::Malformed)]
    #[case::more_used_than_asked(u(100), Some((u(101), u(5), u(1))), ProbeStatus::Malformed)]
    #[case::an_output_past_120_bits(u(100), Some((u(100), U256::from(1) << 120, u(1))), ProbeStatus::Malformed)]
    #[case::an_input_past_120_bits(U256::MAX, Some((U256::from(1) << 120, u(5), u(1))), ProbeStatus::Malformed)]
    fn answers_are_classed(
        #[case] ask: U256,
        #[case] answer: Option<(U256, U256, U256)>,
        #[case] status: ProbeStatus,
    ) {
        assert_eq!(Probe::classify(ask, answer).status, status);
    }

    fn curve_ladder(points: &[(u64, u64)]) -> Ladder {
        let terms = Terms { tiers: Tiers::default(), request: Request::default(), ..sale() };
        let probes: Vec<Probe> = points
            .iter()
            .map(|(x, y)| priced(*x, *y, CURVE))
            .collect();
        Ladder::settle(&probes, &terms, 0)
    }

    fn rung(input: u64, output: u64, segment: Segment) -> Rung {
        Rung { input: u(input), output: u(output), segment }
    }

    #[test]
    fn rungs_are_exact_and_chords_round_down() {
        let l = curve_ladder(&[(10_000, 100_000), (20_000, 190_000), (30_000, 250_001)]);

        assert_eq!(l.output(u(10_000)), Some(u(100_000)));
        assert_eq!(l.output(u(20_000)), Some(u(190_000)));
        assert_eq!(l.output(u(15_000)), Some(u(145_000)));
        assert_eq!(l.output(u(25_001)), Some(u(220_006)));
        assert_eq!(l.output(u(30_001)), None);
    }

    #[test]
    fn nothing_is_quoted_under_the_first_rung() {
        let l = curve_ladder(&[(10_000, 100_000), (20_000, 190_000)]);
        assert_eq!(l.output(u(9_999)), None);
        assert_eq!(l.output(U256::ZERO), None);
    }

    #[test]
    fn a_flat_span_quotes_its_lower_rung() {
        let l = Ladder::from_rungs(vec![
            rung(100, 1_000, Segment::Chord),
            rung(200, 1_900, Segment::Flat),
            rung(400, 3_900, Segment::Chord),
        ]);
        assert_eq!(l.output(u(150)), Some(u(1_000)));
        assert_eq!(l.output(u(199)), Some(u(1_000)));
        assert_eq!(l.output(u(200)), Some(u(1_900)));
        assert_eq!(l.output(u(300)), Some(u(2_900)));
    }

    #[test]
    fn a_ladder_with_a_flat_span_has_no_second_fill() {
        let l = Ladder::from_rungs(vec![
            rung(100, 100, Segment::Chord),
            rung(200, 190, Segment::Flat),
            rung(400, 370, Segment::Chord),
        ]);
        assert!(l.after(u(50)).is_empty());
        assert!(l.after(U256::ZERO).is_empty());
    }

    #[test]
    fn the_haircut_rounds_down_and_keeps_the_order() {
        let terms = Terms {
            rule: Rule::Step,
            tiers: Tiers::default(),
            request: Request::default(),
            ..sale()
        };
        let probes = [priced(10, 1, CURVE), priced(20, 9_999, CURVE), priced(40, 9_999, CURVE)];
        let l = Ladder::settle(&probes, &terms, 2);
        // 1 * 9_998 / 10_000 is 0, so the first rung goes and the next ends no span;
        // 9_999 * 9_998 / 10_000 = 9_997.0002
        assert_eq!(l.rungs(), &[rung(20, 9_997, Segment::Chord), rung(40, 9_997, Segment::Flat)]);
    }

    #[test]
    fn the_shifted_ladder_never_quotes_more_than_is_left() {
        let l = curve_ladder(&[(10_000, 100_000), (20_000, 190_000), (40_000, 350_000)]);
        // bound: rung 20,000 pays 190,000, so 25,000 more input pays at most 350,000 - 190,000
        assert_eq!(l.after(u(15_000)).last(), Some(rung(25_000, 160_000, Segment::Chord)));
        assert!(l.after(u(40_001)).is_empty());
    }

    #[test]
    fn a_fill_on_a_rung_is_bounded_by_the_next_rung() {
        let l = curve_ladder(&[(99, 99), (197, 197), (400, 390), (800, 700)]);
        // on a rung: the rung above bounds the fill, so none of its output is left unreserved
        assert_eq!(l.after(u(197)).rungs(), &[rung(603, 310, Segment::Chord)]);
        assert_eq!(
            l.after(u(99)).rungs(),
            &[rung(301, 193, Segment::Chord), rung(701, 503, Segment::Chord)]
        );
        // at the last rung nothing is above it
        assert!(l.after(u(800)).is_empty());
        // off a rung: the first rung past it bounds the fill, as before
        assert_eq!(
            l.after(u(150)).rungs(),
            &[rung(250, 193, Segment::Chord), rung(650, 503, Segment::Chord)]
        );
    }

    #[test]
    fn rungs_double_up_to_the_cap() {
        assert_eq!(rung_inputs(u(3), None, 4), vec![u(3), u(6), u(12), u(24)]);
        assert_eq!(rung_inputs(u(3), Some(u(10)), 8), vec![u(3), u(6), u(10)]);
        assert_eq!(rung_inputs(U256::ZERO, None, 2), vec![u(1), u(2)]);
    }
}
