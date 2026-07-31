# Changelog

## 0.8.1

### Fixed

- `ContractChange::is_empty` now accounts for `token_balances`, so contract changes carrying only token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).

## 0.2.0

### Updated

- Protobuf struct updated to align with recent changes in the indexer.

### Changed

- Removed the distinction between VM and native implementations. Now, there is a single implementation type that can extract both contracts and protocol state.
- Enabled the attachment of dynamic attributes to protocol components.
