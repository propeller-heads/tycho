# Algebra Integral

Shared indexing for DEXs built on Algebra Integral. One package serves every such DEX: each gets a
manifest per chain and keeps its own protocol system (`nami_v4`, ...).

Deployments:

| DEX | Protocol system | Manifests |
|---|---|---|
| Nami V4 | `nami_v4` | `ethereum-nami-v4.yaml`, `base-nami-v4.yaml`, `robinhood-nami-v4.yaml` |

A manifest sets two params. `map_pools_created` takes `factory` (the DEX's Algebra factory) and
`protocol_type_name` (the pools' protocol type, such as `nami_v4_pool`). `map_protocol_changes`
takes `dci`, which is `true` only on a chain whose indexer runs the Dynamic Contract Indexer: the
package then registers each plugin's `beforeSwap` as an entrypoint and emits the block's storage
changes, so a plugin with no native handler can be simulated in the VM.

Every pool the factory's `Pool` event names is indexed, pools of custom deployers included.

## How a pool's fee is found

An Algebra pool with the `BEFORE_SWAP` flag asks its plugin for the fee of every swap, so a quote
needs the plugin's logic and its configuration. Four terms describe where they come from:

| Term | Meaning |
|---|---|
| Plugin | The contract a pool calls before and after swaps. One per pool, an `AlgebraPluginProxy` (a beacon proxy). |
| Beacon | An `UpgradeableBeacon` shared by many plugins. It names the implementation they all run. |
| Implementation | The code a plugin runs. Simulation picks its Rust handler by `(chain, implementation)`. |
| Module | One feature inside an implementation (dynamic fee, MEVX, farming, ...). Each keeps its configuration at slots fixed by its code. |

The package follows the chain pool → plugin → beacon → implementation and decodes the plugin's
storage, module by module, into attributes on the pool's component:

```
pool
 │  Plugin(address) event
 ▼
plugin ── storage writes ──▶ module slot table ──▶ <module>/<field>
 │  BeaconUpgraded(beacon) event
 ▼
beacon
 │  implementation()
 ▼
implementation ──▶ plugin_impl
```

Example, Robinhood pool `0xd8b0966a7d4b62c96f841a5cf4db2130dba4d16a` read on 2026-11-17:

1. The pool emitted `Plugin(0xb0918bdd…bb8a)`.
2. The plugin's beacon is `0x33e837fa…b9e3`, whose implementation is `0x1a2898f7…6874`. That
   address becomes `plugin_impl`.
3. The plugin wrote slot `0xfbbf…9a00`, which the `dynamic_fee` module lists. The word decodes to
   `dynamic_fee/alpha1` = 0, `dynamic_fee/alpha2` = 0 and `dynamic_fee/base_fee` = 500.
4. Simulation looks `(Robinhood, 0x1a2898f7…6874)` up in its registry, gets the modules that
   implementation is made of, and builds each from its attributes.

The plugin also writes its volatility oracle on every swap: a header slot and one slot of a
65536-slot ring buffer. No module lists those slots, so a swap emits nothing. What is indexed is
the plugin's configuration, some 10 to 20 values that change on owner calls.

## Attributes

The package emits, on the pool's component:

| Attribute | Value | From |
|---|---|---|
| `plugin_impl` | 20-byte address of the implementation the plugin runs | the plugin's read of `beacon.implementation()` in its creation transaction, then the beacon's `Upgraded` events |
| `<module>/<field>` | the field's bytes: big-endian, exactly as wide as the field | the plugin's storage writes, decoded through the module slot table |

Simulation picks the plugin's modules by `(chain, plugin_impl)` and builds each module from its
`<module>/<field>` attributes (`crates/tycho-simulation/src/evm/protocol/algebra_integral/plugin/`).

When a pool names its plugin, every indexed field of each slot the plugin wrote in that transaction
is emitted as a creation, zero values included. Later, only fields whose bytes changed are emitted.
A slot written several times in one transaction counts once, from its first old value to its last
new one. Slots no module lists emit nothing.

## Discovery without RPC

A plugin's beacon and implementation are read from the trace of the transaction that creates the
plugin; no `eth_call` is made and no beacon is listed in a manifest.

| Fact | Source |
|---|---|
| Plugin's beacon | `BeaconUpgraded(beacon)`, logged by the proxy's constructor |
| Implementation at creation | the constructor's own call to `beacon.implementation()` and its return data |
| Later implementations | the beacon's `Upgraded(implementation)` logs |

Checked on Ethereum transaction
`0xa7571666e8df2f53e75dac29aaccff54e1c0e9879ff42f0dd5eb60991944d981`: plugin `0x2f643a73…752e`
logs `BeaconUpgraded(0x44ce18e6…b879)` and its call to that beacon returns `0x6580d2f0…3788`.

This needs extended blocks whose call traces carry return data. That is not yet confirmed for Base
and Robinhood.

## Layout

```
src/plugin/
  storage.rs       StorageField: slot, offset and width of one value, checked at compile time
  attributes.rs    plugin_attributes(writes, Creation | Update)
  beacon.rs        plugin_beacon, beacon_upgrades
  changes.rs       named_plugin_attributes, updated_plugin_attributes, implementation_attribute
  modules/
    mod.rs         PluginModule, MODULES (every indexed module)
    algebra/       modules Algebra publishes
      dynamic_fee.rs
    <dex>/         modules only that DEX deploys
```

`map_protocol_changes` (`src/modules/6_map_protocol_changes.rs`) calls these functions:

| When | What is emitted on the pool |
|---|---|
| The pool's `Plugin` event names a plugin | `named_plugin_attributes`: `plugin_impl` and every indexed field of the slots the plugin wrote in that transaction, as creations |
| The pool's plugin writes a module slot later | `updated_plugin_attributes`: the fields whose bytes changed |
| The pool's plugin logs `BeaconUpgraded` again | `plugin_impl` of the new beacon, read from the plugin's call to it in that transaction; removed when the trace shows no such call |
| A beacon logs `Upgraded` | `plugin_impl` on every pool whose current plugin is bound to that beacon |
| The pool's `Plugin` event replaces a plugin | deletion of `plugin_impl` and of every module attribute, then the new plugin's attributes |

Two stores carry the beacon links (`src/modules/5_store_plugin_beacons.rs`):
`store_plugin_beacons` holds the beacon each plugin is bound to, and `store_beacon_pools` lists
the pools per beacon, spread over 256 keys because a store value is bounded. A pool stays listed
after its plugin moves to another beacon, so an upgrade is applied only to pools whose plugin is
still bound to the upgraded beacon.

A plugin's later writes are found by slot first: only a write to a slot some module lists costs
a store read, and a swap writes none.

## What simulation does with the attributes

| Situation | Chains with the VM fallback (Ethereum) | Other chains |
|---|---|---|
| No plugin, or `BEFORE_SWAP` off | the pool's last fee | same |
| `plugin_impl` is registered | native handler | native handler |
| `plugin_impl` is unknown or missing | VM handler through DCI | decoding fails, the pool is not quoted |
| A module meets a value it cannot model | decoding fails, the pool is not quoted | same |

A pool is never quoted with a guessed fee. A missing or stale attribute therefore stops quotes for
that pool; it does not produce a wrong price.

## Adding a module

A module is indexed when the simulation reads it. Its layout lives in one file on each side, at
the same path:

| Layer | File | Content |
|---|---|---|
| Substreams | `src/plugin/modules/<vendor>/<module>.rs` | the module's slots as constants and a `MODULE` listing its fields |
| Simulation | `plugin/modules/<vendor>/<module>.rs` | a type built from the module's attributes |

`<vendor>` is `algebra` for modules Algebra publishes and the DEX's name for modules only it
deploys.

1. Copy `src/plugin/modules/algebra/dynamic_fee.rs`. Take each slot constant from the contract
   source, not from its namespace string: the constants in deployed modules do not always match
   the ERC-7201 formula in their comments.
2. List only the fields the simulation reads, each as
   `StorageField::new("<module>/<field>", SLOT, offset_in_bytes, width_in_bytes)`, with the offset
   counted from the low end of the word.
3. Add the module to `MODULES` in `src/plugin/modules/mod.rs`. Its tests reject a slot shared by
   two modules, overlapping fields and attribute names without the module's prefix.
4. Add a test that decodes a storage word read from a deployed plugin, naming the plugin and
   block.
5. Bump the package version, release the DEX's spkg and resync that DEX. Other DEXs on the package
   are unaffected unless their plugins contain the module.

The table is applied to every plugin of every DEX. Module slots are hashes of the module's
namespace, so a plugin without the module never writes them.

## What a new DEX delivers

| Case | This package | Simulation |
|---|---|---|
| Its plugins use only indexed modules | a manifest per chain | a registry entry per implementation |
| A new arrangement of indexed modules | nothing | a registry entry |
| A module of its own | a file under `src/plugin/modules/<dex>/`, a line in `MODULES`, a new spkg, a resync of that DEX | the module's file and the DEX's registry entries |
| It upgrades a beacon | nothing, if the layouts are unchanged | a registry entry for the new implementation, merged before the upgrade |

Tests an adopter ships:

1. For each module, a storage word read from a deployed plugin decodes to the expected attributes.
2. Unit tests of each simulation module, edges included.
3. A differential check against a locally synced indexer: for every pool, the handler's quote
   equals `beforeSwap` called from the pool with `eth_call` at the same block, in both directions.
   The attributes come from the indexer, so the check covers the slot table and the simulation
   module together.
4. A range test covering at least one configuration change.

## Limits

- **Plugins that are not beacon proxies** get no `plugin_impl`. Algebra's earlier base plugins are
  deployed per pool with immutables, so an address does not identify their code.
- **Adaptive fee.** `dynamic_fee` indexes the fields of a flat fee. A non-zero alpha needs the
  volatility oracle's recent timepoints, which are not indexed; simulation rejects such a pool.
- **One layout per module.** The table is applied to every plugin of every DEX and module names
  are unique. A deployment whose version of a module uses other slots writes nothing the table
  knows, so its pools fail to decode. Supporting it means a second module entry under another
  name.

## Deployment assumptions

- **Initial configuration.** A plugin's initial configuration is written in the transaction that
  names it to its pool, as the plugin factory does when it creates both. A pool pointed at a plugin
  created in an earlier transaction would miss those writes, so it is paused (`paused` attribute)
  until it names a plugin created in the naming transaction.
- **Beacon proxies.** Only a plugin constructed as a beacon proxy gets a `plugin_impl`. A pool
  without one is simulated through the VM where that is enabled and is not quoted elsewhere.

## Upgrade boundary

| Dependency | Followed how | What breaks |
|---|---|---|
| Plugin implementation behind a beacon | the beacon's `Upgraded` logs update `plugin_impl` on every pool whose plugin is bound to it | simulation rejects an implementation it has no registry entry for, so the pools stop being quoted until one is added |
| Module storage layout | fixed in `src/plugin/modules/` | a new implementation that moves a module's slots or repacks a word needs a new table entry, a new spkg and a resync; until then the old attributes go stale |
| A pool's plugin address | the pool's `Plugin` event | see the initial-configuration assumption |
| A plugin's beacon | the plugin's `BeaconUpgraded` logs | a plugin bound to a beacon without reading its implementation in that transaction loses `plugin_impl` until that beacon is upgraded |
| Nami's fee source contracts | not indexed: simulation ports them and looks them up by the address in `fee_override/source` | a new source contract needs a simulation entry before a pool is pointed at it |

A DEX opens the registry PR for a new implementation before it upgrades its beacon.
