//! Strict mode's streamed range: the pieces that decide what is checked, and what the run covered.
//!
//! The runner subscribes to the indexer while it indexes the range, so every block's deltas go
//! through the production decoder. What it checks is sampled, since checking every block costs
//! too much:
//!
//! - every block's deltas are applied, which costs nothing extra;
//! - simulation checks run at up to [`MAX_SIMULATED_BLOCKS`] blocks in which an expected component
//!   changed, spread evenly over the range ([`Sampler`]);
//! - execution checks run at the fewest of those blocks, spread evenly, that meet the [`Floor`]
//!   ([`blocks_to_execute`]).
//!
//! After the stream, the streamed state must quote exactly as the snapshot at the stop block
//! ([`quote_sheet`], [`sheet_differences`]), and [`Coverage`] says how much the range exercised.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use serde::Serialize;
use tycho_simulation::tycho_common::models::token::Token;

use crate::tvl_only::Quoter;

/// The most blocks the stream runs simulation checks at.
pub const MAX_SIMULATED_BLOCKS: u64 = 50;

/// How much a streamed range must exercise. Report-only for now: a shortfall is a warning until
/// the calibration run confirms the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floor {
    /// Blocks in which an expected component changed.
    pub changed_blocks: u64,
    /// Changed blocks per expected component.
    pub changed_blocks_per_component: u64,
    /// Execution checks, in total.
    pub executions: u64,
    /// Execution checks per component with execution on.
    pub executions_per_component: u64,
}

impl Floor {
    pub const DEFAULT: Floor = Floor {
        changed_blocks: 20,
        changed_blocks_per_component: 2,
        executions: 100,
        executions_per_component: 10,
    };
}

/// Picks the blocks the stream simulates: the range is cut into up to [`MAX_SIMULATED_BLOCKS`]
/// equal slices, and the first changed block of each slice is taken. The picks spread over the
/// range without knowing in advance how many blocks will change.
#[derive(Debug)]
pub struct Sampler {
    start: u64,
    slice: u64,
    taken: BTreeSet<u64>,
}

impl Sampler {
    pub fn new(start_block: u64, stop_block: u64, max_blocks: u64) -> Self {
        let length = stop_block.saturating_sub(start_block) + 1;
        Self {
            start: start_block,
            slice: length.div_ceil(max_blocks.max(1)),
            taken: BTreeSet::new(),
        }
    }

    /// Whether to simulate at `block`, in which an expected component changed.
    pub fn take(&mut self, block: u64) -> bool {
        let slice = block.saturating_sub(self.start) / self.slice;
        self.taken.insert(slice)
    }
}

/// The execution checks one simulated block offers, per component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub block: u64,
    pub executions: BTreeMap<String, u64>,
}

/// The fewest of `candidates`, spread evenly over them, whose executions meet `floor`: in total,
/// and for every component that has executions at all. All of them when even that falls short.
pub fn blocks_to_execute(candidates: &[Candidate], floor: Floor) -> Vec<u64> {
    let blocks: Vec<&Candidate> = candidates
        .iter()
        .sorted_by_key(|candidate| candidate.block)
        .collect();
    let components: BTreeSet<&String> = blocks
        .iter()
        .flat_map(|candidate| candidate.executions.keys())
        .collect();
    let meets = |picked: &[&Candidate]| {
        let mut total = 0;
        let mut by_component: BTreeMap<&String, u64> = BTreeMap::new();
        for candidate in picked {
            for (component, count) in &candidate.executions {
                total += count;
                *by_component
                    .entry(component)
                    .or_default() += count;
            }
        }
        total >= floor.executions &&
            components.iter().all(|component| {
                by_component
                    .get(component)
                    .copied()
                    .unwrap_or(0) >=
                    floor.executions_per_component
            })
    };
    for count in 1..=blocks.len() {
        let picked = spread(&blocks, count);
        if meets(&picked) {
            return picked
                .iter()
                .map(|candidate| candidate.block)
                .collect();
        }
    }
    blocks
        .iter()
        .map(|candidate| candidate.block)
        .collect()
}

/// `count` items of `items`, evenly spaced, the first and the last included.
fn spread<T: Copy>(items: &[T], count: usize) -> Vec<T> {
    match (items.len(), count) {
        (0, _) | (_, 0) => Vec::new(),
        (_, 1) => vec![items[items.len() / 2]],
        (len, count) => (0..count)
            .map(|i| i * (len - 1) / (count - 1))
            .dedup()
            .map(|i| items[i])
            .collect(),
    }
}

/// What a streamed range exercised.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub test: String,
    /// Blocks the stream delivered after its first snapshot.
    pub blocks_streamed: u64,
    /// Of those, the blocks in which an expected component changed.
    pub changed_blocks: u64,
    pub changed_blocks_by_component: BTreeMap<String, u64>,
    pub simulated_blocks: Vec<u64>,
    pub executed_blocks: Vec<u64>,
    pub executions: u64,
    pub executions_by_component: BTreeMap<String, u64>,
    /// Per component, the directions quoted at a simulated block after a delta.
    pub quoted_directions: BTreeMap<String, BTreeSet<String>>,
    /// Per component, the directions that trade at the end of the stream.
    pub tradable_directions: BTreeMap<String, BTreeSet<String>>,
}

impl Coverage {
    /// What falls short of `floor`. `expected` are the components the test expects, and
    /// `executed` those of them with execution on.
    pub fn shortfalls(
        &self,
        floor: Floor,
        expected: &[String],
        executed: &[String],
    ) -> Vec<String> {
        let mut missing = Vec::new();
        if self.changed_blocks < floor.changed_blocks {
            missing.push(format!(
                "{} blocks changed an expected component, under the floor of {}",
                self.changed_blocks, floor.changed_blocks
            ));
        }
        for component in expected {
            let changed = self
                .changed_blocks_by_component
                .get(component)
                .copied()
                .unwrap_or(0);
            if changed < floor.changed_blocks_per_component {
                missing.push(format!(
                    "{component} changed in {changed} blocks, under the floor of {}",
                    floor.changed_blocks_per_component
                ));
            }
        }
        if self.executions < floor.executions {
            missing.push(format!(
                "{} execution checks, under the floor of {}",
                self.executions, floor.executions
            ));
        }
        for component in executed {
            let executions = self
                .executions_by_component
                .get(component)
                .copied()
                .unwrap_or(0);
            if executions < floor.executions_per_component {
                missing.push(format!(
                    "{component} had {executions} execution checks, under the floor of {}",
                    floor.executions_per_component
                ));
            }
        }
        for (component, tradable) in &self.tradable_directions {
            let quoted = self.quoted_directions.get(component);
            let unquoted: Vec<&String> = tradable
                .iter()
                .filter(|direction| quoted.is_none_or(|quoted| !quoted.contains(*direction)))
                .collect();
            if !unquoted.is_empty() {
                missing.push(format!(
                    "{component}: {} never quoted after a delta",
                    unquoted.iter().join(", ")
                ));
            }
        }
        missing
    }
}

/// The fractions of a direction's max input the sheet quotes, in thousandths, as the ladder does.
const LADDER: [u32; 3] = [1, 10, 100];

/// Everything a state answers, by question: each direction's limits and spot price, and quotes at
/// 0.1%, 1% and 10% of its max input after `sizing`, which caps it as the ladder does. Values are
/// printed, so a sheet outlives the state, which a later decode may invalidate.
pub fn quote_sheet<Q: Quoter + ?Sized>(
    state: &Q,
    tokens: &[Token],
    sizing: impl Fn(&BigUint) -> BigUint,
) -> BTreeMap<String, String> {
    let show =
        |result: Result<String, String>| result.unwrap_or_else(|e| format!("an error ({e})"));
    let mut sheet = BTreeMap::new();
    for pair in tokens.iter().permutations(2) {
        let (token_in, token_out) = (pair[0], pair[1]);
        let direction = format!("{} -> {}", token_in.symbol, token_out.symbol);
        let limits = state.limits(token_in, token_out);
        sheet.insert(
            format!("{direction}: limits"),
            show(
                limits
                    .clone()
                    .map(|(input, output)| format!("max input {input}, max output {output}")),
            ),
        );
        sheet.insert(
            format!("{direction}: spot price"),
            show(
                state
                    .spot_price(token_in, token_out)
                    .map(|price| price.to_string()),
            ),
        );
        let Ok((max_input, _)) = limits else {
            continue;
        };
        let max_input = sizing(&max_input);
        for thousandths in LADDER {
            let amount_in = &max_input * thousandths / 1000u32;
            if amount_in.is_zero() {
                continue;
            }
            sheet.insert(
                format!(
                    "{direction}: the quote at {:.1}% of the limit",
                    f64::from(thousandths) / 10.0
                ),
                show(
                    state
                        .amount_out(amount_in.clone(), token_in, token_out)
                        .map(|out| format!("{out} for {amount_in} {}", token_in.symbol)),
                ),
            );
        }
    }
    sheet
}

/// Every question `streamed` and `snapshot` answer differently, or only one of them answers.
pub fn sheet_differences(
    streamed: &BTreeMap<String, String>,
    snapshot: &BTreeMap<String, String>,
) -> Vec<String> {
    streamed
        .keys()
        .chain(snapshot.keys())
        .unique()
        .sorted()
        .filter_map(|question| match (streamed.get(question), snapshot.get(question)) {
            (Some(a), Some(b)) if a == b => None,
            (a, b) => Some(format!(
                "{question}: streamed {}, snapshot {}",
                a.map_or("nothing", String::as_str),
                b.map_or("nothing", String::as_str)
            )),
        })
        .collect()
}

/// How many times each component id occurs in `ids`, by lowercase id.
pub fn count_by_component<'a>(ids: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, u64> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for id in ids {
        *counts
            .entry(id.to_lowercase())
            .or_default() += 1;
    }
    counts.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_simulation::tycho_common::{models::Chain, Bytes};

    use super::*;

    #[test]
    fn the_sampler_takes_the_first_changed_block_of_each_slice() {
        // 1000 blocks in 50 slices of 20.
        let mut sampler = Sampler::new(1000, 1999, 50);
        assert!(sampler.take(1000));
        assert!(!sampler.take(1019));
        assert!(sampler.take(1020));
        assert!(sampler.take(1999));

        // A short range: one slice per block.
        let mut short = Sampler::new(10, 12, 50);
        assert!(short.take(10) && short.take(11) && short.take(12));
        assert!(!short.take(12));
    }

    #[test]
    fn a_busy_range_is_simulated_at_most_fifty_times() {
        let mut sampler = Sampler::new(0, 9_999, MAX_SIMULATED_BLOCKS);
        let taken = (0..10_000)
            .filter(|block| sampler.take(*block))
            .count();
        assert_eq!(taken, 50);
    }

    fn candidate(block: u64, executions: &[(&str, u64)]) -> Candidate {
        Candidate {
            block,
            executions: executions
                .iter()
                .map(|(id, n)| (id.to_string(), *n))
                .collect(),
        }
    }

    #[test]
    fn execution_runs_at_the_fewest_evenly_spread_blocks_that_meet_the_floor() {
        // 24 executions per block over 3 components: 5 blocks reach 100 in total.
        let candidates: Vec<Candidate> = (0..50)
            .map(|i| candidate(1000 + i * 10, &[("0xa", 8), ("0xb", 8), ("0xc", 8)]))
            .collect();
        let picked = blocks_to_execute(&candidates, Floor::DEFAULT);
        assert_eq!(picked, vec![1000, 1120, 1240, 1360, 1490]);
    }

    #[test]
    fn a_component_short_of_its_own_floor_adds_blocks() {
        // 0xb only trades in the last blocks.
        let mut candidates: Vec<Candidate> = (0..10)
            .map(|i| candidate(i, &[("0xa", 30)]))
            .collect();
        candidates.push(candidate(10, &[("0xa", 30), ("0xb", 4)]));
        let picked = blocks_to_execute(&candidates, Floor::DEFAULT);
        assert_eq!(picked, (0..=10).collect::<Vec<_>>(), "0xb reaches only 4, so every block runs");

        let candidates =
            vec![candidate(1, &[("0xa", 60), ("0xb", 10)]), candidate(2, &[("0xa", 60)])];
        assert_eq!(blocks_to_execute(&candidates, Floor::DEFAULT), vec![1, 2]);
        assert!(blocks_to_execute(&[], Floor::DEFAULT).is_empty());
    }

    #[test]
    fn coverage_names_every_shortfall() {
        let coverage = Coverage {
            changed_blocks: 3,
            changed_blocks_by_component: BTreeMap::from([("0xa".to_string(), 3)]),
            executions: 40,
            executions_by_component: BTreeMap::from([("0xa".to_string(), 40)]),
            quoted_directions: BTreeMap::from([(
                "0xa".to_string(),
                BTreeSet::from(["A -> B".to_string()]),
            )]),
            tradable_directions: BTreeMap::from([(
                "0xa".to_string(),
                BTreeSet::from(["A -> B".to_string(), "B -> A".to_string()]),
            )]),
            ..Coverage::default()
        };
        let expected = ["0xa".to_string(), "0xb".to_string()];
        assert_eq!(
            coverage.shortfalls(Floor::DEFAULT, &expected, &expected),
            vec![
                "3 blocks changed an expected component, under the floor of 20",
                "0xb changed in 0 blocks, under the floor of 2",
                "40 execution checks, under the floor of 100",
                "0xb had 0 execution checks, under the floor of 10",
                "0xa: B -> A never quoted after a delta",
            ]
        );
    }

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, 18, 0, &[], Chain::Ethereum, 100)
    }

    /// A constant-product pool over fixed reserves; a zero reserve stops a direction.
    struct Pool(u64, u64);

    impl Quoter for Pool {
        fn limits(&self, token_in: &Token, _: &Token) -> Result<(BigUint, BigUint), String> {
            Ok(match token_in.symbol.as_str() {
                "A" => (self.0.into(), self.1.into()),
                _ => (self.1.into(), self.0.into()),
            })
        }
        fn spot_price(&self, base: &Token, _: &Token) -> Result<f64, String> {
            match base.symbol.as_str() {
                "A" => Ok(self.1 as f64 / self.0 as f64),
                _ => Err("no price".to_string()),
            }
        }
        fn amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            _: &Token,
        ) -> Result<BigUint, String> {
            let (reserve_in, reserve_out) = match token_in.symbol.as_str() {
                "A" => (BigUint::from(self.0), BigUint::from(self.1)),
                _ => (BigUint::from(self.1), BigUint::from(self.0)),
            };
            Ok(&reserve_out * &amount_in / (reserve_in + amount_in))
        }
    }

    fn tokens() -> Vec<Token> {
        vec![
            token("0x00000000000000000000000000000000000000a1", "A"),
            token("0x00000000000000000000000000000000000000b1", "B"),
        ]
    }

    #[test]
    fn a_sheet_answers_limits_spot_price_and_the_ladder_in_every_direction() {
        let sheet = quote_sheet(&Pool(100_000, 200_000), &tokens(), BigUint::clone);
        assert_eq!(sheet.len(), 10);
        assert_eq!(sheet["A -> B: limits"], "max input 100000, max output 200000");
        assert_eq!(sheet["A -> B: the quote at 1.0% of the limit"], "1980 for 1000 A");
        assert_eq!(sheet["B -> A: spot price"], "an error (no price)");
        // A direction with a zero limit is not quoted.
        let one_way = quote_sheet(&Pool(100_000, 0), &tokens(), BigUint::clone);
        assert_eq!(one_way["B -> A: limits"], "max input 0, max output 100000");
        assert!(!one_way.contains_key("B -> A: the quote at 1.0% of the limit"));
    }

    #[test]
    fn sheets_of_the_same_state_match_and_a_drifted_state_differs_by_question() {
        let streamed = quote_sheet(&Pool(100_000, 200_000), &tokens(), BigUint::clone);
        assert!(sheet_differences(&streamed, &streamed).is_empty());
        // A delta applied twice: one reserve off.
        let snapshot = quote_sheet(&Pool(100_000, 201_000), &tokens(), BigUint::clone);
        let differences = sheet_differences(&streamed, &snapshot);
        assert!(
            differences.contains(
                &"A -> B: limits: streamed max input 100000, max output 200000, snapshot max \
                  input 100000, max output 201000"
                    .to_string()
            ),
            "{differences:#?}"
        );
        let mut partial = snapshot.clone();
        partial.remove("A -> B: spot price");
        assert!(sheet_differences(&snapshot, &partial)
            .contains(&"A -> B: spot price: streamed 2.01, snapshot nothing".to_string()));
    }

    #[test]
    fn counts_are_kept_per_lowercase_component() {
        assert_eq!(
            count_by_component(["0xA", "0xa", "0xb"]),
            BTreeMap::from([("0xa".to_string(), 2), ("0xb".to_string(), 1)])
        );
    }
}
