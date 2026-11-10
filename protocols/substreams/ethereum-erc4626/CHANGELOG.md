# Changelog

## v0.2.0

### Added

- Index stUSDS (`0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9`, Sky, USDS).
- Optional per-vault `extra_entrypoints` in `params.json`, traced as DCI entrypoints at the
  vault's first interaction in addition to the standard ones. They index state the standard
  entrypoints can miss: a rate read only when it accrues. stUSDS gets `str()`, spUSDC, spUSDT and
  spETH get `vsr()`, sUSDS and sUSDC get `ssr()`. Before, these vaults decoded only where another
  protocol's trace had happened to read their rate slot.
  `map_protocol_changes` now takes the vault params as well, through the same `&vaults` string
  as `map_protocol_components`.

### Migration

Requires a full resync of the erc4626 extractor (from `initialBlock` 16428133): components are
emitted at their params block and extra entrypoints only at a vault's first interaction, which for
the vaults gaining one runs from sUSDS (block 20677434) to stUSDS (23517637). Before it, add the
MakerDAO Vat
(`0x35d1b3f3d7966a1dfe207aa4514c12a259a0492b`) to the indexer's `DCI_BLACKLIST_ADDRESSES`: see the
Readme's "Deployment" section.
