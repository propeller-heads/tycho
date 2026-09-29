// Copyright (c) 2026 Everlong Labs Limited

//! Parity replays of the Solidity-generated fixtures under `testdata/` (provenance in
//! `testdata/README.md`): every row of every fixture, to the wei and by revert class, with no
//! sampling and no tolerance; and the `ProtocolSim` over the deployed pool's own component
//! snapshots and `eth_call` previews (`protocol_sim`).

mod core;
mod e2e;
mod financing;
mod fixtures;
mod leverage;
mod protocol_sim;
mod swap_hook;
