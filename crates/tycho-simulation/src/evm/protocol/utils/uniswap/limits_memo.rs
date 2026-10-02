use std::sync::{Arc, OnceLock};

use num_bigint::BigUint;
use tycho_common::simulation::errors::SimulationError;

type DirectionLimits = [OnceLock<(BigUint, BigUint)>; 2];

/// The `get_limits` result of each swap direction, computed on first use.
///
/// Clones taken after the first `get_limits` call share one memo; an empty memo allocates
/// nothing, so a post-swap state that is never asked for limits costs nothing. Whatever changes
/// a state's price, liquidity or ticks must give it a fresh memo.
#[derive(Debug, Default, Clone)]
pub(crate) struct LimitsMemo(OnceLock<Arc<DirectionLimits>>);

impl LimitsMemo {
    /// Return the memoized limits of a direction, computing them on first use. Errors are not
    /// memoized.
    pub(crate) fn get_or_compute(
        &self,
        zero_for_one: bool,
        compute: impl FnOnce() -> Result<(BigUint, BigUint), SimulationError>,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let cell = &self.0.get_or_init(Arc::default)[usize::from(zero_for_one)];
        if let Some(limits) = cell.get() {
            return Ok(limits.clone());
        }
        let limits = compute()?;
        Ok(cell.get_or_init(|| limits).clone())
    }
}

/// Derived data: states compare equal whatever their memos hold.
impl PartialEq for LimitsMemo {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for LimitsMemo {}
