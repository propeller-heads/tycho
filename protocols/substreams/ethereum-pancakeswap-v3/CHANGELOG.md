# Changelog

## v0.1.3

- Join tick and pool-liquidity store deltas by store key and ordinal instead of position.
  Writers and consumers share store-key helpers, preserving the unprefixed V3 address format.
- Aggregate tick writes per transaction before building attribute updates. Preserve Creation
  across subsequent updates, omit ticks created then deleted in the same transaction, and
  classify deletion then recreation of an existing tick as Update.
- Remove a redundant reference in a `format!` argument, which current Clippy rejects.
