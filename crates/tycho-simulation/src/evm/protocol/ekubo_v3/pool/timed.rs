#[cfg(not(test))]
use std::time::SystemTime;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use alloy::eips::merge::SLOT_DURATION_SECS;
use ekubo_sdk::quoting::types::TimeRateDelta;
use itertools::Itertools as _;
use num_traits::Zero as _;
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use crate::evm::protocol::ekubo_v3::attributes::rate_deltas_from_attributes;

pub const GAS_COST_OF_ONE_BITMAP_SLOAD: u64 = 2_000;

pub struct TimedTransition {
    pub rate_token0: Option<u128>,
    pub rate_token1: Option<u128>,
    pub last_time: Option<u64>,
    pub time_rate_deltas: Option<Vec<TimeRateDelta>>,
}

pub fn estimate_block_timestamp(
    swapped_this_block: bool,
    last_time: u64,
) -> Result<u64, SimulationError> {
    Ok(if swapped_this_block {
        last_time
    } else {
        // TODO How accurate is it to take the current timestamp?
        Ord::max(last_time + SLOT_DURATION_SECS, current_timestamp()?)
    })
}

pub fn finish_transition(
    last_time: u64,
    deltas: &[TimeRateDelta],
    mut updated_attributes: HashMap<String, Bytes>,
    deleted_attributes: HashSet<String>,
) -> Result<TimedTransition, TransitionError> {
    let (rate_token0, rate_token1, new_last_time) = (
        updated_attributes
            .remove("rate_token0")
            .or_else(|| updated_attributes.remove("token0_sale_rate"))
            .map(u128::try_from)
            .transpose()?,
        updated_attributes
            .remove("rate_token1")
            .or_else(|| updated_attributes.remove("token1_sale_rate"))
            .map(u128::try_from)
            .transpose()?,
        updated_attributes
            .remove("last_time")
            .or_else(|| updated_attributes.remove("last_execution_time"))
            .map(u64::try_from)
            .transpose()?,
    );

    let first_active_delta_idx =
        new_last_time.map_or(0, |last_time| deltas.partition_point(|trd| trd.time <= last_time));

    let changed_deltas = rate_deltas_from_attributes(
        updated_attributes
            .into_iter()
            .chain(
                deleted_attributes
                    .into_iter()
                    .map(|key| (key, Bytes::new())),
            )
            .map(|(key, value)| (key, Cow::Owned(value))),
        new_last_time.unwrap_or(last_time),
    )
    .map_err(TransitionError::DecodeError)?
    .collect_vec();

    Ok(TimedTransition {
        rate_token0,
        rate_token1,
        last_time: new_last_time,
        time_rate_deltas: (!changed_deltas.is_empty() || !first_active_delta_idx.is_zero()).then(
            || {
                let mut new_deltas = deltas[first_active_delta_idx..].to_vec();

                for delta in changed_deltas {
                    let res = new_deltas.binary_search_by_key(&delta.time, |d| d.time);

                    match res {
                        Ok(idx) => {
                            if delta.rate_delta0.is_zero() && delta.rate_delta1.is_zero() {
                                new_deltas.remove(idx);
                            } else {
                                new_deltas[idx] = delta;
                            }
                        }
                        Err(idx) => {
                            new_deltas.insert(idx, delta);
                        }
                    }
                }

                new_deltas
            },
        ),
    })
}

#[cfg(test)]
fn current_timestamp() -> Result<u64, SimulationError> {
    Ok(crate::evm::protocol::ekubo_v3::test_cases::TEST_TIMESTAMP)
}

#[cfg(not(test))]
fn current_timestamp() -> Result<u64, SimulationError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| SimulationError::FatalError(format!("System time before UNIX EPOCH: {e:?}")))
        .map(|d| d.as_secs())
}
