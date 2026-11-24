# Changelog

## v0.1.0

- Plugin storage decoding: the slot tables of known plugin modules and the attributes they emit.
- Pool modules for Algebra Integral 1.2 pools: components from the factory's `Pool` events,
  balances, ticks, liquidity, fee and plugin configuration.
- Plugin wiring: `plugin_impl` and the module attributes of each pool's plugin, followed through
  plugin switches, beacon changes and beacon upgrades; a pool whose plugin was created before it
  was named is paused.
- A plugin switch deletes only the attributes the pool carries and unpauses only a paused pool;
  `store_plugin_beacons` records both. A transfer from a pool to itself gives no balance delta.
- Modules `fee_override`, `anti_sniper` and `mevx` (Nami) and `farming` (Algebra).
- Manifests for Nami V4 on Ethereum, Base and Robinhood, with the `factory`,
  `protocol_type_name`, `modules` and `dci` params.
- A pool that names a plugin gets every field of the deployment's modules, zero for untouched
  slots. The manifest's `modules` param names those modules.
