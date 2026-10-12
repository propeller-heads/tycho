# Changelog

## v0.5.3

- Aggregate tick writes by transaction, pool and tick before building attributes. Preserve
  Creation across later updates, cancel creation followed by deletion, and classify deletion
  then recreation of an existing tick as Update so reorg restores its prior value.
- Fix tick net-liquidity attribute pairing: join store deltas to tick deltas by store key
  and ordinal instead of position. The previous positional zip could swap the value and
  ChangeType between tick_lower and tick_upper of one ModifyLiquidity event.

## v0.5.2

- Remove redundant references in format arguments to retain compatibility with current Clippy.
