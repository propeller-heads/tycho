# Changelog

## v0.1.3

- Fix tick net-liquidity attribute pairing: join net-liquidity store deltas to tick deltas by
  store key and ordinal instead of by position. Both ticks of one Mint or Burn carry that event's
  log ordinal and `store_ticks_liquidity` orders its writes with an unstable sort on the ordinal,
  so the previous positional zip could swap the value and `ChangeType` between the lower and upper
  tick of a single event. The store key is now built in one place, `tick_store_key`, which both
  the writer and the consumer call.
- Remove a redundant reference in a `format!` argument, which current Clippy rejects.
