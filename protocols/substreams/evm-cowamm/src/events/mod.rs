use crate::{
    abi::{
        b_cow_pool::events::{LogExit, LogJoin, Transfer},
        gpv2_settlement::events::Trade,
    },
    pb::cowamm::CowPool,
};
use substreams_ethereum::{pb::eth::v2::Log, Event};
use tycho_substreams::prelude::*;

pub mod exit_pool;
pub mod join_pool;
pub mod trade;
pub mod transfer;
/// A trait for extracting changed balance from an event.
pub trait BalanceEventTrait {
    /// Get all balance deltas from the event.
    ///
    /// # Arguments
    ///
    /// * `tx` - Reference to the `Transaction`.
    /// * `pool` - Reference to the `Pool`.
    /// * `event` - The event. We use it to access the ordinal number of the event (used by the
    ///   balance store to sort), and the address of the event for lp_token Transfer tracking.
    ///
    /// # Returns
    ///
    /// A vector of `BalanceDelta` that represents the balance deltas.
    fn get_balance_delta(&self, tx: &Transaction, pool: &CowPool, event: &Log)
        -> Vec<BalanceDelta>;
}

/// Represent every events of a Cow pool.
pub enum EventType {
    JoinPool(LogJoin),
    ExitPool(LogExit),
    Trade(Trade),
    Transfer(Transfer),
}

impl EventType {
    fn as_event_trait(&self) -> &dyn BalanceEventTrait {
        match self {
            EventType::JoinPool(event) => event,
            EventType::ExitPool(event) => event,
            EventType::Trade(event) => event,
            EventType::Transfer(event) => event,
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
        LogJoin::match_and_decode(event).map(EventType::JoinPool),
        LogExit::match_and_decode(event).map(EventType::ExitPool),
        Trade::match_and_decode(event).map(EventType::Trade),
        Transfer::match_and_decode(event).map(EventType::Transfer),
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
/// * `pool` - Reference to the `CowPool`.
///
/// # Returns
///
/// A vector of `BalanceDelta` that represents
pub fn get_log_changed_balances(
    tx: &Transaction,
    event: &Log,
    pool: &CowPool,
) -> Vec<BalanceDelta> {
    decode_event(event)
        .map(|e| {
            e.as_event_trait()
                .get_balance_delta(tx, pool, event)
        })
        .unwrap_or_default()
}

// pub fn get_deltas_from_bind(log: Log, tx: Transaction) -> Vec<BalanceDelta> {
//     let tx_deltas: Vec<BalanceDelta> = vec![];

//     const BIND_TOPIC: &str =
// "0xe4e1e53800000000000000000000000000000000000000000000000000000000";     const BIND_SELECTOR:
// &str = "e4e1e538";

//     if log.topics.first().map(|t| t.to_hex()) == Some(BIND_TOPIC.to_string()) {
//         // Find the call that contains this log by matching addresses and checking calls
//             let call = tx.calls.iter().find(|call| {
//                 call.address == log.address
//                 && !call.state_reverted
//                 && call.input.len() > 4 //checks if theres data after the function selector, if
// not then theres no data to decode                 && hex::encode(&call.input[..4]) ==
// BIND_SELECTOR //check if its the the
// // right function                                                                     //
// selection selector             })?;
//             let bind = Bind::decode(call).expect("failed to decode bind");
//             let token = bind.token;
//             let amount = bind.balance.to_signed_bytes_be();

//             let bind_tx = tx.into();
//             let delta = BalanceDelta {
//                 ord: bind.ordinal,
//                 tx: Some(Transaction {
//                     from: bind_tx.from.clone(),
//                     to: bind_tx.to.clone(),
//                     hash: bind_tx.hash.clone(), //since the binds happen
//                     index: bind_tx.index,
//                 }),
//                 token: token,
//                 delta: amount,
//                 component_id: bind
//                     .address
//                     .clone()
//                     .to_hex()
//                     .as_bytes()
//                     .to_vec(),
//             };
//             tx_deltas.push(delta);
//     };

//     tx_deltas
// }

// pub fn get_bind_changed_balances(block: Block) -> Vec<BalanceDelta> {
//     let tx_deltas: Vec<BalanceDelta> = vec![];

//     const BIND_TOPIC: &str =
// "0xe4e1e53800000000000000000000000000000000000000000000000000000000";     const BIND_SELECTOR:
// &str = "e4e1e538";

// let binds = block
// .transaction_traces
// .iter()
// // extract (tx, receipt) pairs; skip tx without receipts
// .filter_map(|tx| {
//     tx.receipt
//         .as_ref()
//         .map(|receipt| (tx, receipt))
// })
// // for each (tx, receipt) emit all the matching binds
// .flat_map(|(tx, receipt)| {
//     receipt
//         .logs
//         .iter()
// topic match
//                 .filter(|log| log.topics.first().map(|t| t.to_hex()) ==
// Some(BIND_TOPIC.to_string()))                 // validate log data and map to CowPoolBind
//                 .filter_map(move |log| {
//                     // Find the call that contains this log by matching addresses and checking
// calls                     let call = tx.calls.iter().find(|call| {
//                         call.address == log.address
//                         && !call.state_reverted
//                         && call.input.len() > 4 //checks if theres data after the function
// selector, if not then theres no data to decode                         &&
// hex::encode(&call.input[..4]) == BIND_SELECTOR //check if its the the
// // right function                                                                           //
// selection selector                     })?;
//                     let bind = Bind::decode(call).expect("failed to decode bind");
//                     let token = bind.token;
//                     let amount = bind.balance.to_signed_bytes_be();

//                     let bind_tx = tx.into();
//                     let delta = BalanceDelta {
//                         ord: bind.ordinal,
//                         tx: Some(Transaction {
//                             from: bind_tx.from.clone(),
//                             to: bind_tx.to.clone(),
//                             hash: bind_tx.hash.clone(), //since the binds happen
//                             index: bind_tx.index,
//                         }),
//                         token: token,
//                         delta: amount,
//                         component_id: bind
//                             .address
//                             .clone()
//                             .to_hex()
//                             .as_bytes()
//                             .to_vec(),
//                     };
//                     tx_deltas.push(delta);
//                 })
//         })
//         .collect::<Vec<_>>();

// }

//the problem with trying to extract the binds from the store is
//the number of times the for loop will run i've already tried this and i was
//getting duplicate results i do not want to go with this method so i'll
//treat the binds as a normal balance delta i extract from a transaction

// let is_pool_creation = log.address == factory_address &&
//     log.topics.first().map(|t| t.to_hex()) ==
//         Some(COWAMM_POOL_CREATED_TOPIC.to_string());
// if is_pool_creation {
//     // Handle pool creation
//     let pool_address_topic = match log.topics.get(1) {
//         Some(topic) => topic.as_slice()[12..].to_vec(),
//         None => continue,
//     };

//     let pool_address_hex = hex::encode(&pool_address_topic);
//     let pool_key = format!("Pool:0x{}", pool_address_hex);

//     let bind_data = match binds.get_first(&pool_address_hex) {
//         Some(data) => data,
//         None => continue,
//     };

//     let parsed_binds = match parse_binds(&bind_data) {
//         Some(binds) if !binds.is_empty() => binds,
//         _ => continue,
//     };
//     //replace with get_bind_changed_balances() helper

//     for bind in parsed_binds.iter() {
//         let bind_tx = bind.tx.as_ref().unwrap();
//         let delta = BalanceDelta {
//             ord: bind.ordinal,
//             tx: Some(Transaction {
//                 from: bind_tx.from.clone(),
//                 to: bind_tx.to.clone(),
//                 hash: bind_tx.hash.clone(), //since the binds happen
//                 index: bind_tx.index,
//             }),
//             token: bind.token.clone(),
//             delta: BigInt::from_unsigned_bytes_be(&bind.amount).to_signed_bytes_be(),
//             component_id: bind
//                 .address
//                 .clone()
//                 .to_hex()
//                 .as_bytes()
//                 .to_vec(),
//         };
//         tx_deltas.push(delta);
//     }}

//we will use the pool_created_topic for getting pools

//get all pool contract creations from the bcowfactory log event - some might not be finalized so
// untradable but doesnt matter get all the binds and store them
//create the pools from the binds to create a CowPool

//for pools that dont have bind or were not created with binds then treat the first JOIN_POOL()
// event as the bind to extract tokens joined to the pool
//
// ok now how do we get the weight?

//666652312312453 (with decimals as 18 decimals, Wrapped Ether on xDai (WETH))

//10431962237316783 (with decimals as 18 decimals, Gnosis Token on xDai (GNO))

//100×11,098,614,549,629,236666,652,312,312,453​≈6.0066%

// 100×11,098,614,549,629,23610,431,962,237,316,783​≈93.9934%

//These are not proper weights for pools in CowAMM lets just leave them

//forget about this case
