// Copyright (c) 2026 Everlong Labs Limited

//! Financing parity: Morpho Blue, the `AdaptiveCurveIrm`, `MorphoBlueAccount`, `MMRouterLib` /
//! `MMRouter`, `FLAMMGateLib` and the `FLAMMSwapLib` settlement legs, replayed over the
//! Solidity-generated module fixtures, the edge fixtures, the settlement edges and the stateful
//! sequences of the Go port (`testdata/README.md` section 2). Every row is compared to the wei,
//! reverts by class.

mod common;
mod financing_edges;
mod module_fixtures;
mod sequences;
mod settlement_edges;
