//! One side of a listing as the door quoted it: exact outputs at sampled input sizes, and what
//! may be quoted between them.

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::utils::solidity_math::mul_div;

/// The door's quotes on one side at increasing input sizes: `(amount in, amount out)`, strictly
/// increasing in both. Empty where the side quoted nothing.
///
/// Between two neighbouring samples it quotes their chord, rounded down, which is a lower bound
/// on the fill wherever the output is concave in the input. The door rounds each output down, so
/// the samples of a concave output can show a chord a little steeper than the one before it; past
/// what that rounding explains, the output is not concave there, and the stretch quotes the
/// smaller sample's output instead, which an output that grows with the input never undercuts.
///
/// On a credit listing the samples are two runs: the sizes its pool's custody fills, then the
/// sizes its credit route fills, each at the door's own quote. The output may step where the
/// route takes over, so nothing is quoted strictly between the two runs, and each run's chords
/// are judged on its own samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "LadderWire", into = "LadderWire")]
pub(super) struct Ladder {
    points: Vec<(U256, U256)>,
    /// Per stretch (between point `i` and `i + 1`): whether it quotes the chord or holds the
    /// output of point `i`.
    chords: Vec<bool>,
    /// The index of the first point the credit route fills: those before it fill inside the
    /// pool's custody, those from it through the route. `None` where every point fills inside
    /// custody, as on every listing without a credit mode.
    route_from: Option<usize>,
}

/// A ladder as it is serialized: its points alone where every one fills inside custody, the form
/// every ladder had before credit listings were served, and with the index of its first routed
/// point otherwise.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum LadderWire {
    Custody(Vec<(U256, U256)>),
    Stepped { points: Vec<(U256, U256)>, route_from: usize },
}

/// The path a fill takes, which its gas follows: inside its pool's custody, or, on a credit
/// listing, through the pool's credit route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FillPath {
    Custody,
    Route,
}

/// Why a ladder does not quote an amount.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The side quoted nothing.
    Empty,
    /// Below the smallest sampled size: the door's minimum is not known below it.
    BelowSmallest(U256),
    /// Above the largest sampled size, which is the most the listing filled at the read, and the
    /// path that fill takes.
    AboveLargest { amount_in: U256, amount_out: U256, path: FillPath },
    /// Between the largest size a credit listing's custody fills and the smallest its route was
    /// sampled at, where the output may step: neither sample bounds a fill there. The largest
    /// custody size's output rides along, the most of the amount the listing fills.
    AcrossStep { custody_most: U256, custody_out: U256, route_least: U256 },
}

impl Ladder {
    /// Fails for points that are not strictly increasing in both input and output, and for a
    /// first routed point past the last point.
    pub(super) fn new(
        points: Vec<(U256, U256)>,
        route_from: Option<usize>,
    ) -> Result<Self, String> {
        if points
            .first()
            .is_some_and(|(input, output)| input.is_zero() || output.is_zero())
        {
            return Err("a sample of zero input or output".into());
        }
        for pair in points.windows(2) {
            let ((a0, o0), (a1, o1)) = (pair[0], pair[1]);
            if a1 <= a0 || o1 <= o0 {
                return Err(format!("samples ({a0}, {o0}) and ({a1}, {o1}) do not both increase"));
            }
        }
        if let Some(from) = route_from.filter(|&from| from >= points.len()) {
            return Err(format!("a first routed point {from} past the {} points", points.len()));
        }
        // Each run's first stretch quotes its chord, having none before it to be judged against;
        // the stretch across the step is never quoted.
        let starts_run = |i: usize| i == 0 || route_from == Some(i);
        let mut chords = Vec::with_capacity(points.len().saturating_sub(1));
        for i in 0..points.len().saturating_sub(1) {
            let concave = starts_run(i) || !steeper(&points[i - 1..=i + 1]);
            chords.push(concave);
        }
        Ok(Self { points, chords, route_from })
    }

    /// The ladder of `samples`, `(size, output or None where the door refused it)`, in
    /// increasing size: the run of answered sizes from the smallest one, ending at the first
    /// refused size or the first output that does not grow. Nothing past that end is quoted, so a
    /// door that refuses a middle size is not quoted beyond it. On a credit listing, every size at
    /// or above `route_from` fills through the credit route and every one below it inside the
    /// pool's custody; `None` where none routes.
    pub(super) fn from_samples(samples: &[(U256, Option<U256>)], route_from: Option<U256>) -> Self {
        let mut points: Vec<(U256, U256)> = Vec::new();
        for &(input, output) in samples {
            match output {
                Some(output) if !output.is_zero() && !input.is_zero() => {
                    if points
                        .last()
                        .is_some_and(|&(a, o)| input <= a || output <= o)
                    {
                        break;
                    }
                    points.push((input, output));
                }
                _ if points.is_empty() => {}
                _ => break,
            }
        }
        let route_from = route_from
            .map(|least| points.partition_point(|&(input, _)| input < least))
            .filter(|&from| from < points.len());
        Self::new(points, route_from).expect("the run kept is strictly increasing")
    }

    pub(super) fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Every sample kept, in increasing size.
    #[cfg(test)]
    pub(super) fn points(&self) -> &[(U256, U256)] {
        &self.points
    }

    /// The smallest sample.
    pub(super) fn first(&self) -> Option<(U256, U256)> {
        self.points.first().copied()
    }

    /// The largest sample: the most the side fills, and its output.
    pub(super) fn last(&self) -> Option<(U256, U256)> {
        self.points.last().copied()
    }

    /// Whether a point fills through the credit route.
    #[cfg(test)]
    pub(super) fn routes(&self) -> bool {
        self.route_from.is_some()
    }

    /// The path the fill of point `i` takes.
    fn path(&self, i: usize) -> FillPath {
        if self
            .route_from
            .is_some_and(|from| i >= from)
        {
            FillPath::Route
        } else {
            FillPath::Custody
        }
    }

    /// Whether `amount_in` is the largest sample the pool's custody fills: the side's last, or
    /// the one before its route begins.
    pub(super) fn is_custody_most(&self, amount_in: U256) -> bool {
        let most = match self.route_from {
            Some(from) => from.checked_sub(1),
            None => self.points.len().checked_sub(1),
        };
        most.is_some_and(|i| self.points[i].0 == amount_in)
    }

    /// The output for `amount_in`, and the path its fill takes.
    pub(super) fn quote(&self, amount_in: U256) -> Result<(U256, FillPath), Refusal> {
        let (Some(&(first_in, _)), Some(&(last_in, last_out))) =
            (self.points.first(), self.points.last())
        else {
            return Err(Refusal::Empty);
        };
        if amount_in < first_in {
            return Err(Refusal::BelowSmallest(first_in));
        }
        if amount_in > last_in {
            return Err(Refusal::AboveLargest {
                amount_in: last_in,
                amount_out: last_out,
                path: self.path(self.points.len() - 1),
            });
        }
        // The first sample at or above the amount; the one before it starts the stretch.
        let upper = self
            .points
            .partition_point(|&(input, _)| input < amount_in);
        let (a1, o1) = self.points[upper];
        let path = self.path(upper);
        if a1 == amount_in || upper == 0 {
            return Ok((o1, path));
        }
        let (a0, o0) = self.points[upper - 1];
        if self.route_from == Some(upper) {
            return Err(Refusal::AcrossStep { custody_most: a0, custody_out: o0, route_least: a1 });
        }
        if !self.chords[upper - 1] {
            return Ok((o0, path));
        }
        // The run is below the stretch's input, so the rise is below its output and cannot
        // overflow; were it to fail, the stretch's lower output is still a floor on the fill.
        Ok((mul_div(amount_in - a0, o1 - o0, a1 - a0).map_or(o0, |rise| o0 + rise), path))
    }
}

/// Whether the second stretch of three points rises faster than the first by more than the
/// door's rounding explains. Each output is the door's rounded down, so it lies less than one unit
/// under the unrounded one: a concave output's samples satisfy
/// `(o2 - o1 - 1) / (a2 - a1) <= (o1 - o0 + 1) / (a1 - a0)`, and only samples that break it are
/// steeper. A strict comparison would hold the lower output over stretches where a straight run
/// of the door's prices merely rounded differently at its ends.
fn steeper(points: &[(U256, U256)]) -> bool {
    let [(a0, o0), (a1, o1), (a2, o2)] = points else {
        return false;
    };
    // The outputs strictly increase and the first is not zero, so neither bound wraps; the
    // products are cross-multiplied without overflow.
    let one = U256::from(1u8);
    let later: U512 = (*o2 - *o1 - one).widening_mul(*a1 - *a0);
    let earlier: U512 = (*o1 - *o0 + one).widening_mul(*a2 - *a1);
    later > earlier
}

impl TryFrom<LadderWire> for Ladder {
    type Error = String;

    fn try_from(wire: LadderWire) -> Result<Self, Self::Error> {
        match wire {
            LadderWire::Custody(points) => Self::new(points, None),
            LadderWire::Stepped { points, route_from } => Self::new(points, Some(route_from)),
        }
    }
}

/// A ladder's points, its route mark dropped.
impl From<Ladder> for Vec<(U256, U256)> {
    fn from(ladder: Ladder) -> Self {
        ladder.points
    }
}

impl From<Ladder> for LadderWire {
    fn from(ladder: Ladder) -> Self {
        match ladder.route_from {
            None => Self::Custody(ladder.points),
            Some(route_from) => Self::Stepped { points: ladder.points, route_from },
        }
    }
}

impl From<Refusal> for SimulationError {
    fn from(refusal: Refusal) -> Self {
        let message = match refusal {
            Refusal::Empty => "the listing fills nothing on this side".to_string(),
            Refusal::BelowSmallest(smallest) => {
                format!("amount below the smallest size the listing was quoted at, {smallest}")
            }
            Refusal::AboveLargest { amount_in, .. } => {
                format!("amount above the most the listing fills, {amount_in}")
            }
            Refusal::AcrossStep { custody_most, route_least, .. } => format!(
                "amount between the most the listing fills from its pool's custody, \
                 {custody_most}, and the least its credit route was quoted at, {route_least}"
            ),
        };
        SimulationError::InvalidInput(message, None)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn u(value: u64) -> U256 {
        U256::from(value)
    }

    fn points(points: &[(u64, u64)]) -> Vec<(U256, U256)> {
        points
            .iter()
            .map(|&(a, o)| (u(a), u(o)))
            .collect()
    }

    fn ladder(of: &[(u64, u64)]) -> Ladder {
        Ladder::new(points(of), None).unwrap()
    }

    /// A credit side: 100 in for 190 out and 200 for 370 inside custody, then 400 for 600, 800
    /// for 1,100 and 1,600 for 2,000 through the route, a step down in price past custody.
    fn stepped() -> Ladder {
        Ladder::new(
            points(&[(100, 190), (200, 370), (400, 600), (800, 1_100), (1_600, 2_000)]),
            Some(2),
        )
        .unwrap()
    }

    /// 100 in for 190 out, 200 for 370, 400 for 700: a concave side.
    fn concave() -> Ladder {
        ladder(&[(100, 190), (200, 370), (400, 700)])
    }

    #[rstest]
    #[case::the_smallest_sample(100, 190)]
    #[case::a_middle_sample(200, 370)]
    #[case::the_largest_sample(400, 700)]
    // 190 + 50 * 180 / 100 = 280.
    #[case::the_first_chord(150, 280)]
    // 370 + 1 * 330 / 200 = 371.65, rounded down.
    #[case::rounded_down(201, 371)]
    fn a_concave_side_quotes_samples_exactly_and_chords_rounded_down(
        #[case] amount_in: u64,
        #[case] expected: u64,
    ) {
        assert_eq!(concave().quote(u(amount_in)), Ok((u(expected), FillPath::Custody)));
    }

    #[test]
    fn nothing_is_quoted_outside_the_samples() {
        let side = concave();

        assert_eq!(side.quote(u(99)), Err(Refusal::BelowSmallest(u(100))));
        assert_eq!(
            side.quote(u(401)),
            Err(Refusal::AboveLargest {
                amount_in: u(400),
                amount_out: u(700),
                path: FillPath::Custody
            })
        );
        assert_eq!(Ladder::from_samples(&[], None).quote(u(1)), Err(Refusal::Empty));
    }

    /// The second stretch (200 to 400 for 100 more out each 100 in) rises faster than the first
    /// (10 out for 100 in), so it holds the output of its lower sample.
    #[test]
    fn a_stretch_steeper_than_the_one_before_holds_its_lower_output() {
        let side = ladder(&[(100, 100), (200, 110), (400, 310)]);

        assert_eq!(side.quote(u(150)), Ok((u(105), FillPath::Custody)));
        assert_eq!(side.quote(u(300)), Ok((u(110), FillPath::Custody)));
        assert_eq!(side.quote(u(400)), Ok((u(310), FillPath::Custody)));
    }

    /// A straight run of prices, rounded down by the door: `floor(2a / 3)` at 10, 20, 40 and 80
    /// gives 6, 13, 26 and 53, whose last chord (27 over 40) is steeper than the one before it
    /// (13 over 20) only through the rounding, so it still quotes the chord.
    #[test]
    fn a_chord_steeper_only_by_the_doors_rounding_is_still_quoted() {
        let side = ladder(&[(10, 6), (20, 13), (40, 26), (80, 53)]);

        // 26 + 20 * 27 / 40 = 39.5, rounded down; the door pays floor(120 / 3) = 40.
        assert_eq!(side.quote(u(60)), Ok((u(39), FillPath::Custody)));
    }

    #[rstest]
    // The door's minimum refuses the two smallest sizes.
    #[case::refused_small_sizes_are_skipped(
        &[(25, None), (50, None), (100, Some(190)), (200, Some(370))],
        &[(100, 190), (200, 370)]
    )]
    // A refused middle size ends the run.
    #[case::a_refused_middle_size_ends_it(
        &[(100, Some(190)), (200, None), (400, Some(700))],
        &[(100, 190)]
    )]
    // An output that does not grow ends the run.
    #[case::an_output_that_does_not_grow_ends_it(
        &[(100, Some(190)), (200, Some(190)), (400, Some(700))],
        &[(100, 190)]
    )]
    #[case::a_zero_output_is_no_sample(&[(100, Some(0)), (200, Some(370))], &[(200, 370)])]
    #[case::every_size_refused(&[(100, None), (200, None)], &[])]
    fn the_samples_kept_are_the_run_from_the_smallest_answered_size(
        #[case] samples: &[(u64, Option<u64>)],
        #[case] expected: &[(u64, u64)],
    ) {
        let samples: Vec<_> = samples
            .iter()
            .map(|&(a, o)| (u(a), o.map(u)))
            .collect();

        assert_eq!(Ladder::from_samples(&samples, None), ladder(expected));
    }

    #[rstest]
    #[case::a_zero_input(&[(0, 1)])]
    #[case::a_zero_output(&[(1, 0)])]
    #[case::inputs_not_increasing(&[(2, 1), (2, 3)])]
    #[case::outputs_not_increasing(&[(1, 3), (2, 3)])]
    fn points_that_do_not_increase_are_refused(#[case] points: &[(u64, u64)]) {
        assert!(Ladder::new(super::tests::points(points), None).is_err());
    }

    /// A ladder is read back through the same checks it was built with.
    #[test]
    fn a_ladder_round_trips_through_serde_and_refuses_bad_points() {
        let side = concave();
        let json = serde_json::to_string(&side).unwrap();

        assert_eq!(serde_json::from_str::<Ladder>(&json).unwrap(), side);
        assert_eq!(
            json,
            serde_json::to_string(&points(&[(100, 190), (200, 370), (400, 700)])).unwrap()
        );
        let bad = serde_json::to_string(&vec![(u(2), u(1)), (u(1), u(2))]).unwrap();
        assert!(serde_json::from_str::<Ladder>(&bad).is_err());
    }

    /// Amounts near the top of `uint256` neither overflow the chord nor the steepness check.
    #[test]
    fn large_amounts_do_not_overflow() {
        let big = U256::from(1u8) << 250;
        let side = Ladder::new(
            vec![(big, big), (big * u(2), big + u(10)), (big * u(3), big + u(20))],
            None,
        )
        .unwrap();

        assert_eq!(side.quote(big + big / u(2)), Ok((big + u(5), FillPath::Custody)));
    }

    /// A credit side quotes its custody run and its route run each exactly and along its own
    /// chords, each with its path, and nothing strictly between the two.
    #[rstest]
    #[case::the_smallest_custody_sample(100, Ok((190, FillPath::Custody)))]
    // 190 + 50 * 180 / 100 = 280.
    #[case::a_custody_chord(150, Ok((280, FillPath::Custody)))]
    #[case::the_largest_custody_sample(200, Ok((370, FillPath::Custody)))]
    #[case::just_past_custody(201, Err((200, 370, 400)))]
    #[case::just_short_of_the_route(399, Err((200, 370, 400)))]
    #[case::the_smallest_routed_sample(400, Ok((600, FillPath::Route)))]
    // 600 + 200 * 500 / 400 = 850: the route run's first stretch is judged on its own.
    #[case::a_route_chord(600, Ok((850, FillPath::Route)))]
    #[case::the_largest_routed_sample(1_600, Ok((2_000, FillPath::Route)))]
    fn a_credit_side_quotes_each_run_and_nothing_across_the_step(
        #[case] amount_in: u64,
        #[case] expected: Result<(u64, FillPath), (u64, u64, u64)>,
    ) {
        let expected = expected
            .map(|(out, path)| (u(out), path))
            .map_err(|(custody_most, custody_out, route_least)| Refusal::AcrossStep {
                custody_most: u(custody_most),
                custody_out: u(custody_out),
                route_least: u(route_least),
            });

        assert_eq!(stepped().quote(u(amount_in)), expected);
    }

    /// The route run's first stretch (400 to 800 for 320 more) rises far faster than the stretch
    /// across the step (200 to 400 for 10 more), which is never quoted: it is judged on the
    /// route's own samples, and quotes its chord.
    #[test]
    fn each_run_judges_its_chords_on_its_own_samples() {
        let side = Ladder::new(
            points(&[(100, 190), (200, 370), (400, 380), (800, 700), (1_600, 1_000)]),
            Some(2),
        )
        .unwrap();

        // 380 + 200 * 320 / 400 = 540.
        assert_eq!(side.quote(u(600)), Ok((u(540), FillPath::Route)));
        // 700 + 400 * 300 / 800 = 850.
        assert_eq!(side.quote(u(1_200)), Ok((u(850), FillPath::Route)));
    }

    /// Past the largest sample, the most the side fills is handed back with that sample's path.
    #[test]
    fn the_most_a_credit_side_fills_carries_its_path() {
        assert_eq!(
            stepped().quote(u(1_601)),
            Err(Refusal::AboveLargest {
                amount_in: u(1_600),
                amount_out: u(2_000),
                path: FillPath::Route
            })
        );
    }

    /// The first routed point is the first point kept at or above the least routed size; a
    /// route whose points the run did not keep marks none, and every point may route.
    #[rstest]
    #[case::custody_then_route(
        &[(100, Some(190)), (200, Some(370)), (400, Some(600)), (800, Some(1_100))],
        Some(400),
        Some(2)
    )]
    #[case::every_point_routes(&[(100, Some(190)), (200, Some(370))], Some(100), Some(0))]
    #[case::leading_refusals_skipped(&[(50, None), (100, Some(190)), (200, Some(370))], Some(100), Some(0))]
    // A route paying less than custody's last output ends the run there.
    #[case::a_falling_route_is_not_kept(
        &[(100, Some(190)), (200, Some(370)), (400, Some(300))],
        Some(400),
        None
    )]
    #[case::a_refusal_before_the_route_cuts_it(
        &[(100, Some(190)), (200, None), (400, Some(600))],
        Some(400),
        None
    )]
    #[case::no_route(&[(100, Some(190)), (200, Some(370))], None, None)]
    fn the_route_starts_at_the_first_routed_point_kept(
        #[case] samples: &[(u64, Option<u64>)],
        #[case] route_from: Option<u64>,
        #[case] expected: Option<usize>,
    ) {
        let samples: Vec<_> = samples
            .iter()
            .map(|&(a, o)| (u(a), o.map(u)))
            .collect();

        let side = Ladder::from_samples(&samples, route_from.map(u));

        assert_eq!(side.route_from, expected);
        assert_eq!(side.routes(), expected.is_some());
    }

    /// A credit side is read back with its first routed point, through the same checks; a ladder
    /// of custody points alone is still the bare list of points, and reads back as one.
    #[test]
    fn a_credit_side_round_trips_through_serde_and_refuses_a_bad_mark() {
        let side = stepped();
        let json = serde_json::to_string(&side).unwrap();

        assert_eq!(serde_json::from_str::<Ladder>(&json).unwrap(), side);
        let past = serde_json::json!({ "points": points(&[(100, 190)]), "route_from": 1 });
        assert!(serde_json::from_value::<Ladder>(past).is_err());
        let custody = serde_json::to_value(points(&[(100, 190), (200, 370)])).unwrap();
        assert!(!serde_json::from_value::<Ladder>(custody)
            .unwrap()
            .routes());
    }
}
