# Changelog

## v0.1.0

Initial integration of Tempest, a propAMM quoting from lanes committed to the shared
Flashbots `PrioUpdateRegistry`.

### Added

- Indexes `PairRegistered` into one component per pair, identified by
  `keccak256(router, token0, token1)`. The router is mixed in because component ids are
  chain-global: `keccak256(token0, token1)` alone is the venue's own `laneFor` value and
  already collides with `ethereum-fermiswap`'s WETH/USDT component.
- Pins simulation to the maker's committed quote via the `override_block_timestamp`
  attribute, resolved from the registry `updateState` lane index through the
  `lane:{laneFor}` mapping recorded at registration.
- Sources the vault from `VaultUpdated` rather than configuration, so a vault migration is
  followed. A rotation re-reads every tracked token's balance, and `balance_owner` is
  repointed on the existing components.
- Tracks the router's `Pausable` flag. A pair registered while the router is paused is
  reported paused, since every quote entrypoint is `whenNotPaused`.
- Registers the router implementation through DCI rather than the component contract set,
  which is frozen at creation and would go stale on the next `upgradeToAndCall`.
- Reads lanes from `0xda7afeed021eafc1c1af9c362de477dad0396b81`, the `PrioUpdateRegistry`
  the router switched to at block 25989123. The switch is a plain storage write and the
  router emits no events, so it cannot be followed the way `VaultUpdated` is followed for
  the vault: a further migration needs the `registry_address` parameter changed. Lanes
  committed to the previous instance before that block are not indexed.
