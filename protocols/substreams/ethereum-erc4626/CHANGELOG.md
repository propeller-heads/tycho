# Changelog

## v0.2.0

### Added

- Index stUSDS (`0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9`, Sky, USDS).
- Optional per-vault `extra_entrypoints` in `params.json`, traced as DCI entrypoints at the
  vault's first interaction in addition to the standard ones. They index state the standard
  entrypoints can miss, such as stUSDS's `str`, read only when its rate accrues.
  `map_protocol_changes` now takes the vault params as well, through the same `&vaults` string
  as `map_protocol_components`.

### Migration

Requires a resync of the erc4626 extractor from at least stUSDS's creation block 23219535, since
components are emitted at their params block and extra entrypoints at a vault's first interaction
(23517637 for stUSDS). Before it, add the MakerDAO Vat
(`0x35d1b3f3d7966a1dfe207aa4514c12a259a0492b`) to the indexer's `DCI_BLACKLIST_ADDRESSES`: see the
Readme's "Deployment" section.
