# Changelog

## v0.1.3

### Changed

- Replace the configured dynamic swap fee modules with the current Base and Aerodrome
  deployments:
  - `0x090b2A6bb475c00e2256e2095A60887cD710803b`
  - `0xF4Ecd78EBEB6d36CF7f80B5B6B41453515fe2785`
- Keep fee module selection explicit in the SPKG parameters instead of following Factory module
  changes dynamically. A future module rotation requires updating both the SPKG configuration and
  the Tycho Simulation allowlist.
- Add support for the upgraded dynamic fee configuration fields
  `dfc_initialFeeEnabled` and `dfc_initialFee`, including the corresponding set, disable, and
  reset events.
- Accumulate events from the statically configured fee modules in a Substreams store keyed by pool
  and attribute. The first event observed for a pool emits all five configuration fields together
  with `dynamic_fee_module`; fields absent from the configured module are emitted as zero so stale
  attributes from a retired module are replaced. Later events emit only the fields changed by that
  event.
- Add the `dynamic_fee_module` pool attribute as a version marker. The corresponding Tycho
  Simulation release only consumes dynamic fee attributes when the marker matches one of the
  configured modules and otherwise uses the default fee behavior.
- Remove the database backfill utility in favor of the rollback and Substreams back-processing
  rollout described below.

### Migration: database rollback and back-processing

This release is deployed by restoring a complete, internally consistent Tycho database snapshot
from April 1, 2026, before either configured replacement module emitted dynamic fee updates. The
extractor then restarts with the v0.1.3 SPKG and replays every block after the restored cursor. A
separate SQL backfill is not required because the replay begins before the replacement-module
history that this package needs to index.

The restored `aerodrome_slipstreams` extraction height must be lower than block `44_221_569`, the
earliest deployment block among the configured modules. Restore the whole database snapshot,
including protocol state, blocks, transactions, and extraction state; changing only the extractor
cursor would mix state from different points in chain history.

During replay:

1. Substreams back-processes the package's stores from initial block `13_843_704`, reconstructing
   the existing pool registry before replacement-module events are handled.
2. The first configured-module event for a pool emits `dynamic_fee_module`, `dfc_baseFee`,
   `dfc_scalingFactor`, `dfc_feeCap`, `dfc_initialFeeEnabled`, and `dfc_initialFee` together. Fields
   not set by the replacement module are emitted as zero, clearing stale retired-module values in
   the restored database.
3. Later events for the same pool emit only the fields changed by that event, preserving partial
   update semantics.
4. Pools never configured by a replacement module keep no matching marker, so the corresponding
   Tycho Simulation release ignores their stale attributes and uses the default fee behavior.

Use this rollout sequence:

1. Stop the Slipstreams extractor and prevent Simulation from serving partially replayed state.
2. Restore the complete April 1 database snapshot and verify the restored extractor height is below
   `44_221_569`.
3. Build the v0.1.3 SPKG without an initial-block override. Before deployment, run
   `substreams info <package>` and verify every module reports initial block `13_843_704`.
4. Start the extractor with the new SPKG at the restored cursor plus one and allow Substreams
   back-processing and chain replay to reach the current finalized head.
5. Deploy the corresponding Tycho Simulation release only after the extractor is fully caught up.

Because the shared WASM binary changes the hashes of existing stores, a provider without matching
cached results may need to back-process roughly 30 million blocks. Pre-warm the package or budget
for this initialization before restarting production traffic.

This migration restores the correct current state after the Factory rotations, but it is not an
exact reconstruction of the short historical interval between the April 1 snapshot and those
rotations. The v0.1.3 package listens only to the replacement modules, so retired-module updates in
that interval are not replayed, while replacement-module configuration written before Factory
activation is indexed immediately. Keep Simulation isolated until replay reaches the current head.
