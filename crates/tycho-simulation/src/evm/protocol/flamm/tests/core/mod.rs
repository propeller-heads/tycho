// Copyright (c) 2026 Everlong Labs Limited

//! Pool core parity: `FLAMMSwapLib`, `FLAMMLeverLib` and `PriceFeed` over the composed state,
//! replayed against the deployed pool's recorded behaviour (`testdata/README.md` section 4: the
//! Go suite's `TestCoreE2EPreviewGrids`, `TestCoreE2ESequences`, `TestCoreEdgeGrid` and
//! `TestCoreEdgeSequences`, without sampling), plus the flows over scripted seam doubles.

pub(super) mod common;
mod e2e;
mod plan_logic;
mod probes;
