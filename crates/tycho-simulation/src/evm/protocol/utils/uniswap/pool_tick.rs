use std::{fmt, sync::OnceLock};

use alloy::primitives::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tycho_common::simulation::errors::SimulationError;

use super::tick_math::{check_sqrt_price_in_range, get_tick_at_sqrt_ratio};

/// A pool's current tick.
///
/// A swap that stops between two ticks leaves the tick as whatever `get_tick_at_sqrt_ratio`
/// returns for its final sqrt price. That tick is computed on first read, because most post-swap
/// states are dropped without it ever being read. Equality, `Debug` and serde all see the tick
/// value alone, exactly as for a plain `i32`.
#[derive(Clone)]
pub(crate) enum PoolTick {
    Known(i32),
    AtSqrtPrice { sqrt_price: U256, tick: OnceLock<i32> },
}

impl PoolTick {
    /// The tick of an in-range sqrt price, failing exactly as `get_tick_at_sqrt_ratio` does.
    pub(crate) fn at_sqrt_price(sqrt_price: U256) -> Result<Self, SimulationError> {
        check_sqrt_price_in_range(sqrt_price)?;
        Ok(Self::AtSqrtPrice { sqrt_price, tick: OnceLock::new() })
    }

    pub(crate) fn value(&self) -> i32 {
        match self {
            Self::Known(tick) => *tick,
            Self::AtSqrtPrice { sqrt_price, tick } => *tick.get_or_init(|| {
                get_tick_at_sqrt_ratio(*sqrt_price)
                    .expect("an in-range sqrt price always maps to a tick")
            }),
        }
    }
}

impl From<i32> for PoolTick {
    fn from(tick: i32) -> Self {
        Self::Known(tick)
    }
}

impl Default for PoolTick {
    fn default() -> Self {
        Self::Known(i32::default())
    }
}

impl PartialEq for PoolTick {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}

impl Eq for PoolTick {}

impl fmt::Debug for PoolTick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.value(), f)
    }
}

impl Serialize for PoolTick {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PoolTick {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        i32::deserialize(deserializer).map(Self::Known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::protocol::utils::uniswap::tick_math::{
        get_sqrt_ratio_at_tick, MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK,
    };

    #[test]
    fn test_lazy_tick_matches_eager_tick() {
        let mut sqrt_prices = vec![MIN_SQRT_RATIO, MAX_SQRT_RATIO - U256::from(1u64)];
        for tick in
            [MIN_TICK, -887_000, -200_000, -120, -1, 0, 1, 60, 200_000, 887_000, MAX_TICK - 1]
        {
            let at_tick = get_sqrt_ratio_at_tick(tick).unwrap();
            sqrt_prices.extend([at_tick - U256::from(1u64), at_tick, at_tick + U256::from(1u64)]);
        }
        sqrt_prices.retain(|sqrt_price| *sqrt_price >= MIN_SQRT_RATIO);

        for sqrt_price in sqrt_prices {
            let eager = get_tick_at_sqrt_ratio(sqrt_price).unwrap();
            let lazy = PoolTick::at_sqrt_price(sqrt_price).unwrap();

            assert_eq!(format!("{lazy:?}"), format!("{eager:?}"));
            assert_eq!(
                serde_json::to_string(&lazy).unwrap(),
                serde_json::to_string(&eager).unwrap()
            );
            assert_eq!(lazy, PoolTick::from(eager));
            assert_eq!(lazy.value(), eager);
        }
    }

    #[test]
    fn test_lazy_tick_fails_like_eager_tick_out_of_range() {
        for sqrt_price in [U256::ZERO, MIN_SQRT_RATIO - U256::from(1u64), MAX_SQRT_RATIO] {
            let eager = get_tick_at_sqrt_ratio(sqrt_price).unwrap_err();
            let lazy = PoolTick::at_sqrt_price(sqrt_price).unwrap_err();

            assert_eq!(format!("{lazy:?}"), format!("{eager:?}"));
        }
    }

    #[test]
    fn test_known_tick_round_trips_through_serde() {
        let tick = PoolTick::from(-887_272);

        let json = serde_json::to_string(&tick).unwrap();
        let decoded: PoolTick = serde_json::from_str(&json).unwrap();

        assert_eq!(json, "-887272");
        assert_eq!(decoded.value(), -887_272);
    }
}
