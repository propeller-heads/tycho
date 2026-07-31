use std::{borrow::Cow, collections::hash_map::Entry};

use rustc_hash::FxHashMap;

use crate::{
    model::executors::Executor,
    params::{ParamKey, ParamValue},
};

/// The model generates far more valuable information
/// than just the suspicious [Outcome](crate::Outcome)s it writes to stdout.
/// Each [worker_thread](crate::worker::worker_thread) captures this information
/// in a [Telemetry] struct.
/// Once the simulation is complete, the [Telemetry] returned
/// by each [worker_thread](crate::worker::worker_thread) are [merged](Telemetry::merge_into)
/// into one and written as a report to stderr.
///
/// The report contains, for example, whether a specific combination of [ParamKey] and [ParamValue]
/// never produced a success, whether a specific [Executor] combination never produced
/// a success, or how many times a specific [Executor] combination reverted
/// for a specific reason.
///
/// [Telemetry]'s [Display](std::fmt::Display) implementation writes a detailed report.
#[derive(Default)]
pub struct Telemetry {
    executors_to_telemetry: rustc_hash::FxHashMap<Vec<Executor>, ExecutorsTelemetry>,

    key_and_value_to_count_simulated: rustc_hash::FxHashMap<(ParamKey, ParamValue), u64>,
    key_and_value_to_count_success: rustc_hash::FxHashMap<(ParamKey, ParamValue), u64>,
}

impl Telemetry {
    /// Record that [simulate](crate::simulate) completed for a [Params](crate::params::Params)
    /// that contained `key`-`value` pair.
    ///
    /// Used to detect `key`-`value` pairs for which [simulate](crate::simulate)
    /// never completes successfully.
    pub fn param_simulated(&mut self, key: &ParamKey, value: &ParamValue) {
        *self
            .key_and_value_to_count_simulated
            .entry((*key, value.clone()))
            .or_insert(0) += 1;
    }

    /// Record that [simulate](crate::simulate) completed successfully for a
    /// [Params](crate::params::Params) that contained `key`-`value` pair.
    ///
    /// Used to detect `key`-`value` pairs for which [simulate](crate::simulate)
    /// never completes successfully.
    pub fn param_success(&mut self, key: &ParamKey, value: &ParamValue) {
        *self
            .key_and_value_to_count_success
            .entry((*key, value.clone()))
            .or_insert(0) += 1;
    }

    /// Record that [simulate](crate::simulate) completed successfully for a
    /// [Params](crate::params::Params) that contained the given `executors`.
    pub fn executors_success(&mut self, executors: Vec<Executor>) {
        self.executors_to_telemetry
            .entry(executors)
            .or_default()
            .success += 1;
    }

    /// Record that [simulate](crate::simulate) completed successfully for a
    /// [Params](crate::params::Params) that contained the given `executors` and that the
    /// [Outcome](crate::Outcome) was considered suspicious.
    pub fn executors_suspicious(&mut self, executors: Vec<Executor>) {
        self.executors_to_telemetry
            .entry(executors)
            .or_default()
            .suspicious += 1;
    }

    /// Record that [simulate](crate::simulate) completed with [Revert](crate::Error::Revert) for a
    /// [Params](crate::params::Params) that contained the given `executors`.
    pub fn executors_revert(&mut self, executors: Vec<Executor>, reason: Cow<'static, str>) {
        *self
            .executors_to_telemetry
            .entry(executors)
            .or_default()
            .revert_to_count
            .entry(reason)
            .or_insert(0) += 1;
    }

    /// Record that [simulate](crate::simulate) completed with [Ignore](crate::Error::Ignore) for a
    /// [Params](crate::params::Params) that contained the given `executors`.
    pub fn executors_ignore(&mut self, executors: Vec<Executor>, reason: Cow<'static, str>) {
        *self
            .executors_to_telemetry
            .entry(executors)
            .or_default()
            .ignored_to_count
            .entry(reason)
            .or_insert(0) += 1;
    }

    /// Record that [simulate](crate::simulate) completed with [Warning](crate::Error::Warning) for
    /// a [Params](crate::params::Params) that contained the given `executors`.
    pub fn executors_warning(&mut self, executors: Vec<Executor>, reason: Cow<'static, str>) {
        *self
            .executors_to_telemetry
            .entry(executors)
            .or_default()
            .warning_to_count
            .entry(reason)
            .or_insert(0) += 1;
    }

    /// Each worker thread will gather its own [Telemetry] and return it.
    /// Synchronizing worker threads to gather into a shared [Telemetry]
    /// would be very wasteful due to synchronization overhead.
    /// A way is needed to merge the separate [Telemetry]s into one.
    /// This function provides that functionality.
    pub fn merge_into(&mut self, mut other: Self) {
        for (executors, statistics) in other.executors_to_telemetry.drain() {
            match self
                .executors_to_telemetry
                .entry(executors)
            {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().merge_into(statistics);
                }
                Entry::Vacant(entry) => {
                    entry.insert(statistics);
                }
            }
        }
        for (key_value, count) in other
            .key_and_value_to_count_simulated
            .drain()
        {
            *self
                .key_and_value_to_count_simulated
                .entry(key_value)
                .or_insert(0) += count;
        }
        for (key_value, count) in other
            .key_and_value_to_count_success
            .drain()
        {
            *self
                .key_and_value_to_count_success
                .entry(key_value)
                .or_insert(0) += count;
        }
    }
}

/// Writes a detailed report
impl std::fmt::Display for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "# Telemetry")?;

        writeln!(f)?;
        writeln!(f, "## Reverts by executor combination")?;
        writeln!(f)?;
        for (executors, telemetry) in self.executors_to_telemetry.iter() {
            if !telemetry.revert_to_count.is_empty() {
                writeln!(f, "- {executors:?}")?;
                for (reason, count) in &telemetry.revert_to_count {
                    writeln!(f, "    - {reason:?}: {count}")?;
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "## Ignored by executor combination")?;
        writeln!(f)?;
        for (executors, telemetry) in self.executors_to_telemetry.iter() {
            if !telemetry.ignored_to_count.is_empty() {
                writeln!(f, "- {executors:?}")?;
                for (reason, count) in &telemetry.ignored_to_count {
                    writeln!(f, "    - {reason:?}: {count}")?;
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "## Success count by executor combination")?;
        writeln!(f)?;
        for (executors, telemetry) in self.executors_to_telemetry.iter() {
            writeln!(f, "- {executors:?}: {}", telemetry.success)?;
        }

        writeln!(f)?;
        writeln!(f, "## Warnings by executor combination")?;
        writeln!(f)?;
        for (executors, telemetry) in self.executors_to_telemetry.iter() {
            if !telemetry.warning_to_count.is_empty() {
                writeln!(f, "- {executors:?}")?;
                for (reason, count) in &telemetry.warning_to_count {
                    writeln!(f, "    - {reason:?}: {count}")?;
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "## Suspicious count by executor combination")?;
        writeln!(f)?;
        for (executors, telemetry) in self.executors_to_telemetry.iter() {
            writeln!(f, "- {executors:?}: {}", telemetry.suspicious)?;
        }

        writeln!(f)?;
        writeln!(f, "## Param Key Value combinations for which all simulations reverted")?;
        writeln!(f)?;

        for key_value in self
            .key_and_value_to_count_simulated
            .keys()
        {
            if !self
                .key_and_value_to_count_success
                .contains_key(key_value)
            {
                writeln!(f, "- {key_value:?}")?;
            }
        }

        writeln!(f)?;
        writeln!(f, "## Executor combinations without successes")?;
        writeln!(f)?;

        let mut combinations = Vec::new();
        match crate::config::SWAP_COUNT {
            1 => {
                for a in Executor::VARIANTS {
                    combinations.push(vec![a]);
                }
            }
            2 => {
                for a in Executor::VARIANTS {
                    for b in Executor::VARIANTS {
                        combinations.push(vec![a, b]);
                    }
                }
            }
            _ => unimplemented!("SWAP_COUNT > 2 is not implemented"),
        }
        for executors in combinations.iter() {
            let is_without_successes = match self
                .executors_to_telemetry
                .get(executors)
            {
                Some(telemetry) => telemetry.success == 0,
                None => true,
            };

            if is_without_successes {
                writeln!(f, "- {executors:?}")?;
            }
        }
        Ok(())
    }
}

/// The telemetry associated with a list of [Executor]s.
#[derive(Default)]
struct ExecutorsTelemetry {
    success: u64,
    suspicious: u64,
    revert_to_count: FxHashMap<std::borrow::Cow<'static, str>, u64>,
    ignored_to_count: FxHashMap<std::borrow::Cow<'static, str>, u64>,
    warning_to_count: FxHashMap<std::borrow::Cow<'static, str>, u64>,
}

impl ExecutorsTelemetry {
    fn merge_into(&mut self, mut other: Self) {
        self.success += other.success;
        self.suspicious += other.suspicious;
        for (reason, count) in other.revert_to_count.drain() {
            *self
                .revert_to_count
                .entry(reason)
                .or_insert(0) += count;
        }
        for (reason, count) in other.ignored_to_count.drain() {
            *self
                .ignored_to_count
                .entry(reason)
                .or_insert(0) += count;
        }
        for (reason, count) in other.warning_to_count.drain() {
            *self
                .warning_to_count
                .entry(reason)
                .or_insert(0) += count;
        }
    }
}
