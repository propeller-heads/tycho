//! Shared pieces of the seed writers.
//!
//! A seed writer is a native binary that lives next to a Substreams package (`<package>/seed/`)
//! and reconstructs the protocol's state at one block from some source, such as an archive node
//! or another indexer, into the package's seed body. This crate holds what every writer needs
//! regardless of protocol: the seed file layout ([`mod@format`]), reading and writing seed files
//! ([`mod@file`]), archive-node access ([`rpc`]), the manifest rewrite that moves a package's
//! `initialBlock` to the seed block ([`manifest`]) and the subcommands every writer offers
//! ([`cli`]). What a seed contains and how it is reconstructed stays with the package.

pub mod cli;
pub mod file;
pub mod manifest;
pub mod rpc;

pub use tycho_seed_format as format;
