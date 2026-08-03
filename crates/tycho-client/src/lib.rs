//! # Tycho Client
//!
//! This is the client implementation for the Tycho-Indexer, a high-performance indexing service.
//!
//! ## Snapshot+Updates Pattern
//!
//! The Tycho-Indexer expects clients to connect using the snapshot+deltas pattern. This approach
//! aims to provide the most up-to-date and accurate state of data at any given point in time.
//!
//! The core concept is that the client first retrieves a "snapshot" of the current state of data
//! from the server using the rpc methods. This snapshot may consist of any relevant data that
//! is important for the client. After receiving the initial snapshot, the client then continually
//! receives smaller delta updates, which represent changes or modifications made after the snapshot
//! was taken.
//!
//! This pattern is efficient and reduces the load on both the client and server when dealing with
//! large amounts of data. The client only needs to handle the heavy payload once (the snapshot),
//! while subsequent updates are smaller and more manageable. It also ensures that the client always
//! has the most recent version of the data without needing to poll or request it from the server
//! constantly.
//!
//! The following modules implement the different parts of the client:
//!
//! - `rpc` module provides utilities for retrieving snapshots, and associated data such as tokens.
//! - `deltas` module handles receiving and processing delta messages from the server.
const TYCHO_SERVER_VERSION: &str = "v1";

pub mod cli;
pub mod client_metadata;
pub mod deltas;
pub mod feed;
pub mod rpc;
pub mod stream;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

pub use deltas::{DeltasError, WsDeltasClient};
pub use rpc::{HttpRPCClient, RPCError, SnapshotParameters};
