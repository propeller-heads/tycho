use crate::{
    abi::pool::events::{PoolAddLiquidity, PoolRemoveLiquidity, PoolSwap},
    pb::maverick::v2::Pool,
};
use substreams_ethereum::{pb::eth::v2::Log, Event};
use tycho_substreams::prelude::*;

pub mod add_liquidity;
pub mod remove_liquidity;
pub mod swap;

/// A trait for extracting changed balance from an event.
pub trait BalanceEventTrait {
    /// Get all balance deltas from the event.
    ///
    /// # Arguments
    ///
    /// * `tx` - Reference to the `Transaction`.
    /// * `pool` - Reference to the `Pool`.
    /// * `ordinal` - The ordinal number of the event. This is used by the balance store to sort the
    ///
    /// # Returns
    ///
    /// A vector of `BalanceDelta` that represents the balance deltas.
    fn get_balance_delta(&self, tx: &Transaction, pool: &Pool, ordinal: u64) -> Vec<BalanceDelta>;
}

/// Represent every events of a Maverick pool.
pub enum EventType {
    PoolSwap(PoolSwap),
    AddLiquidity(PoolAddLiquidity),
    RemoveLiquidity(PoolRemoveLiquidity),
}

impl EventType {
    fn as_event_trait(&self) -> &dyn BalanceEventTrait {
        match self {
            EventType::PoolSwap(event) => event,
            EventType::AddLiquidity(event) => event,
            EventType::RemoveLiquidity(event) => event,
        }
    }
}

/// Decodes the event from the log.
///
/// # Arguments
///
/// * `event` - A reference to the `Log`.
///
/// # Returns
///
/// An `Option` that contains the `EventType` if the event is recognized.
pub fn decode_event(event: &Log) -> Option<EventType> {
    [
        PoolSwap::match_and_decode(event).map(EventType::PoolSwap),
        PoolAddLiquidity::match_and_decode(event).map(EventType::AddLiquidity),
        PoolRemoveLiquidity::match_and_decode(event).map(EventType::RemoveLiquidity),
    ]
    .into_iter()
    .find_map(std::convert::identity)
}

/// Gets the changed balances from the log.
///
/// # Arguments
///
/// * `tx` - Reference to the `Transaction`.
/// * `event` - Reference to the `Log`.
/// * `pool` - Reference to the `Pool`.
///
/// # Returns
///
/// A vector of `BalanceDelta` that represents
pub fn get_log_changed_balances(tx: &Transaction, event: &Log, pool: &Pool) -> Vec<BalanceDelta> {
    decode_event(event)
        .map(|e| {
            e.as_event_trait()
                .get_balance_delta(tx, pool, event.ordinal)
        })
        .unwrap_or_default()
}
