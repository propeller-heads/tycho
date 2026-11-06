# Changelog

## v0.2.0

### Added

- Index stUSDS (`0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9`, Sky, USDS).
- Optional per-vault `extra_entrypoints` in `params.json`, traced as DCI entrypoints at the
  vault's first interaction in addition to the standard ones. They index state the standard
  entrypoints can miss, such as stUSDS's `str`, read only when its rate accrues.
  `map_protocol_changes` now takes the vault params as well.
