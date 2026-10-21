//! Request-for-quote venues: a market maker is the counterparty and commits to one trade.
//!
//! A venue belongs here when it can decline a specific trade after pricing it: the taker asks
//! for a quote, a maker answers with a signed commitment to fill that trade at that price until
//! expiry, and the settlement contract checks the commitment. The book feeds publish the makers'
//! indicative levels; the states obtain the binding quote through `IndicativelyPriced` at
//! execution time. Venues whose pool fills anyone who arrives with fresh price data, even data
//! fetched per trade, are pAMMs and live in [`crate::pamm`].

pub(crate) mod constants;
pub(crate) mod errors;
pub mod protocols;
