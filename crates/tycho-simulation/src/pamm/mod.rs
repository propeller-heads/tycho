//! Proprietary AMMs served as book feeds: a pool is the counterparty, priced off-chain.
//!
//! A venue belongs here when its pool fills any taker who arrives with fresh price data, whether
//! that data is streamed as a book or fetched per trade as a signed oracle update. The venue's
//! signature, where there is one, attests to a price; it does not commit a maker to one trade.
//! Venues where a maker can decline a specific trade after pricing it are RFQs and live in
//! [`crate::rfq`]. Titan's price-level stream serves pAMMs too but keeps its own module,
//! `price_level_stream` (behind its own cargo feature), with a `Stream`-based API.

pub mod protocols;
