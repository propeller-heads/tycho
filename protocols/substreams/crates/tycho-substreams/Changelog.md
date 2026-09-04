# Changelog

## 0.9.0

### Added

- `seed` module: reads the seed file a package embeds to start from a snapshot (header check
  against the package name and the streamed block, synthetic transaction for the seeded state).
  The file layout lives in the new `tycho-seed-format` crate.

## 0.8.1

### Fixed

- `ContractChange::is_empty` now accounts for `token_balances`, so contract changes carrying only token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).

## 0.2.0

### Updated

- Protobuf struct updated to align with recent changes in the indexer.

### Changed

- Removed the distinction between VM and native implementations. Now, there is a single implementation type that can extract both contracts and protocol state.
- Enabled the attachment of dynamic attributes to protocol components.
