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
//! (`tycho_test::checks::sheet`), and [`Coverage`] says how much the range exercised.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use itertools::Itertools;
use serde::Serialize;

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

    #[test]
    fn counts_are_kept_per_lowercase_component() {
        assert_eq!(
            count_by_component(["0xA", "0xa", "0xb"]),
            BTreeMap::from([("0xa".to_string(), 2), ("0xb".to_string(), 1)])
        );
    }
}
