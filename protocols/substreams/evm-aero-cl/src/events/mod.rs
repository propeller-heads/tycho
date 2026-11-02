use crate::{
    abi::pool::events::{
        Burn, Collect, CollectFees, Flash, IncreaseObservationCardinalityNext, Initialize, Mint,
        Swap,
    },
    pb::tycho::evm::aerodrome::Pool,
};
use substreams_ethereum::{
    pb::eth::v2::{Log, StorageChange},
    Event,
};
use tycho_substreams::{
    models::{BalanceDelta, Transaction},
    prelude::Attribute,
};

pub mod burn;
pub mod collect;
pub mod collect_fees;
pub mod flash;
pub mod increase_observation_cardinality_next;
pub mod initialize;
pub mod mint;
pub mod swap;

/// A trait for extracting changed attributes and balance from an event.
pub trait EventTrait {
    /// Get all relevant changed attributes from the `[StorageChange]`.
    /// If an attribute is changed multiple times, only the last state will be returned.
    ///
    /// # Arguments
    ///
    /// * `storage_changes` - A slice of `StorageChange` that indicates the changes in storage.
    /// * `pool` - Reference to the `Pool`.
    ///
    /// # Returns
    ///
    /// A vector of `Attribute` that represents the changed attributes.
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        pool_address: &[u8; 20],
    ) -> Vec<Attribute>;

    /// Get all balance deltas from the event.
    ///
    /// # Arguments
    ///
    /// * `pool` - Reference to the `Pool`.
    /// * `ordinal` - The ordinal number of the event. This is used by the balance store to sort the
    ///   balance deltas in the correct order.
    ///
    /// # Returns
    ///
    /// A vector of `BalanceDelta` that represents the balance deltas.
    fn get_balance_delta(&self, tx: &Transaction, pool: &Pool, ordinal: u64) -> Vec<BalanceDelta>;
}

/// Represent every events of a UniswapV3 pool.
pub enum EventType {
    Initialize(Initialize),
    Swap(Swap),
    Flash(Flash),
    Mint(Mint),
    Burn(Burn),
    Collect(Collect),
    CollectFees(CollectFees),
    IncreaseObservationCardinalityNext(IncreaseObservationCardinalityNext),
}

impl EventType {
    fn as_event_trait(&self) -> &dyn EventTrait {
        match self {
            EventType::Initialize(e) => e,
            EventType::Swap(e) => e,
            EventType::Flash(e) => e,
            EventType::Mint(e) => e,
            EventType::Burn(e) => e,
            EventType::Collect(e) => e,
            EventType::CollectFees(e) => e,
            EventType::IncreaseObservationCardinalityNext(e) => e,
        }
    }
}

/// Decodes a given log into an `EventType`.
///
/// # Arguments
///
/// * `event` - A reference to the `Log`.
///
/// # Returns
///
/// An `Option<EventType>` that represents the decoded event type.
pub fn decode_event(event: &Log) -> Option<EventType> {
    [
        Initialize::match_and_decode(event).map(EventType::Initialize),
        Swap::match_and_decode(event).map(EventType::Swap),
        Flash::match_and_decode(event).map(EventType::Flash),
        Mint::match_and_decode(event).map(EventType::Mint),
        Burn::match_and_decode(event).map(EventType::Burn),
        Collect::match_and_decode(event).map(EventType::Collect),
        CollectFees::match_and_decode(event).map(EventType::CollectFees),
        IncreaseObservationCardinalityNext::match_and_decode(event)
            .map(EventType::IncreaseObservationCardinalityNext),
    ]
    .into_iter()
    .find_map(std::convert::identity)
}

/// Gets the changed attributes from the log.
///
/// # Arguments
///
/// * `event` - A reference to the `Log`.
/// * `storage_changes` - A slice of `StorageChange` that indicates the changes in storage.
/// * `pool` - Reference to the `Pool` structure.
///
/// # Returns
///
/// A vector of `Attribute` that represents the changed attributes.
pub fn get_log_changed_attributes(
    event: &Log,
    storage_changes: &[StorageChange],
    pool_address: &[u8; 20],
) -> Vec<Attribute> {
    decode_event(event)
        .map(|e| {
            e.as_event_trait()
                .get_changed_attributes(storage_changes, pool_address)
        })
        .unwrap_or_default()
}

/// Gets the changed balances from the log.
///
/// # Arguments
///
/// * `tx` - Reference to the `Transaction` structure.
/// * `event` - A reference to the `Log`.
/// * `pool` - Reference to the `Pool` structure.
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
