# Algebra Integral

Shared indexing for DEXs built on Algebra Integral. One package serves every such DEX: each gets a
manifest per chain and keeps its own protocol system (`nami_v4`, ...).

Deployments:

| DEX | Protocol system | Manifests |
|---|---|---|
| Nami V4 | `nami_v4` | `ethereum-nami-v4.yaml`, `base-nami-v4.yaml`, `robinhood-nami-v4.yaml` |

A manifest sets two params. `map_pools_created` takes `factory` (the DEX's Algebra factory) and
`protocol_type_name` (the pools' protocol type, such as `nami_v4_pool`). `map_protocol_changes`
takes `modules` (see below) and `dci`, which is `true` only on a chain whose indexer runs the
Dynamic Contract Indexer: the package then registers each plugin's `beforeSwap` as an entrypoint
and emits the block's storage changes, so a plugin with no native handler can be simulated in the
VM.

Only pools the factory's `Pool` event names are indexed. An Algebra factory announces the pools
of custom deployers with `CustomPool`, which the package does not read: Nami creates none.

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
| `sqrt_price_x96`, `tick` | the pool's price, big-endian, and its current tick, signed big-endian | `Initialize` and `Swap` events; zero at creation |
| `liquidity` | the pool's active liquidity, big-endian | `Mint` and `Burn` over the current tick, and `Swap`; zero at creation |
| `ticks/<i>` | net liquidity of initialized tick `i`, signed big-endian; created when the tick's gross liquidity turns positive, deleted when it returns to zero | `Mint` and `Burn` events |
| `fee` | the pool's stored fee, used when the plugin does not override it | `Fee` events; zero at creation |
| `plugin` | 20-byte address of the pool's plugin: absent until the pool's first `Plugin` event, zero after one that names no plugin | `Plugin` events |
| `plugin_config` | the pool's plugin flags, one byte | `PluginConfig` events; zero at creation |
| `paused` | present, with value `1`, while the pool names a plugin whose configuration is not known | see [Wiring the pool modules](#wiring-the-pool-modules) |
| `plugin_impl` | 20-byte address of the implementation the plugin runs | the plugin's read of `beacon.implementation()` in its creation transaction, then the beacon's `Upgraded` events |
| `<module>/<field>` | the field's bytes: big-endian, exactly as wide as the field | the plugin's storage writes, decoded through the module slot table |

Simulation picks the plugin's modules by `(chain, plugin_impl)` and builds each module from its
`<module>/<field>` attributes (`crates/tycho-simulation/src/evm/protocol/algebra_integral/plugin/`).

When a pool names its plugin, every field of the deployment's modules is emitted as a creation:
with the value the plugin wrote in that transaction, or zero for a slot it left untouched. The
plugin is created in that transaction, so zero is what its storage holds there. A pool therefore
carries every field its modules can read from the start, and a field that is missing on a pool is
one the package does not index, never one that happens to be zero.

Later, only fields whose bytes changed are emitted. A slot written several times in one
transaction counts once, from its first old value to its last new one. Slots none of the
deployment's modules lists emit nothing.

## The `modules` param

A manifest names the modules its deployment's plugins are built from, in the order their
attributes are emitted:

```
modules=dynamic_fee,farming,fee_override,mevx
```

`modules_named` resolves the names against `MODULES` and fails on an empty or missing param, on a
name the package does not index and on one listed twice; the pool modules stop on that error, so
a manifest with a wrong or missing list does not run. The list is per manifest: a chain where a
DEX deploys a plugin variant with one more module lists that module there only. A plugin without a listed module never writes its slots and
carries its fields as zero, which no handler of that plugin's implementation reads.

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

This needs extended blocks whose call traces carry return data. Base and Robinhood have them, as
read through the StreamingFast endpoints:

| Chain | Transaction | Block | `implementation()` returned |
|---|---|---|---|
| Base | `0x9f8e853bec912732ceee1d9e2f2a62a810b8908d0e5de4a385c7cb1c58da14fa` | 52483466 | `0x0c534dd0…05d1` |
| Robinhood | `0x18cba2f58ea0dbb3d3b6482c3377c1ffd15636c7b5c0a086cd569e2f73297ded` | 86164187 | `0x8788decb…ba97` |

None of the three returned implementations is the one its beacon points at today: each of Nami's
beacons has been upgraded since. Following `Upgraded` is on the path of every sync from the start
block.

## Layout

```
src/modules/       the pool modules, numbered by their stage in the module graph
  1_map_pool_created.rs               components from the factory's Pool events
  2_store_pools.rs                    each pool's tokens
  3_map_events.rs                     the pool events the later modules read
  4_map_and_store_balance_changes.rs  balances from Transfer logs
  4_map_and_store_ticks.rs            net and gross liquidity per tick
  4_map_and_store_liquidity.rs        active liquidity
  4_store_pool_plugins.rs             each pool's plugin, and the reverse
  5_store_plugin_beacons.rs           beacon links, and what each pool carries
  6_map_protocol_changes.rs           everything above as BlockChanges, and the plugin wiring
src/plugin/
  storage.rs       StorageField: slot, offset and width of one value, checked at compile time
  attributes.rs    creation_attributes, update_attributes: writes and modules to attributes
  beacon.rs        plugin_beacon, beacon_upgrades
  changes.rs       named_plugin_attributes, updated_plugin_attributes, implementation_attribute
  modules/
    mod.rs         PluginModule, MODULES (every indexed module), modules_named
    algebra/       modules Algebra publishes
      dynamic_fee.rs
      farming.rs
    nami/          modules only Nami deploys
      anti_sniper.rs
      fee_override.rs
      mevx.rs
    <dex>/         modules only that DEX deploys
```

A manifest carries only what differs per deployment, such as the factory address and the
`modules` param. Slots are constants in code.

## Wiring the pool modules

The pool modules resolve `modules` once and then, per transaction:

1. **A pool names a plugin** (its `Plugin` event): call `named_plugin_attributes`. `Some` holds
   the pool's `plugin_impl` and module attributes. `None` means the transaction did not create
   the plugin, so its configuration is not known: pause the pool.
2. **A pool switches plugin**: delete the module attributes and `plugin_impl` the pool carries
   for the old plugin, and only those, before applying step 1 to the new one. Storage rejects
   the deletion of an attribute it has no row for, so the modules have to know what a pool
   carries. A pool paused under step 1 is unpaused when it names a plugin created in the naming
   transaction, or no plugin. Naming the plugin a pool already names changes nothing.
3. **A tracked plugin writes storage**: call `updated_plugin_attributes` for the pool that runs it.
4. **A beacon logs `Upgraded`** (`beacon_upgrades`): update `plugin_impl` on every pool whose
   plugin is bound to that beacon. Keep the beacon's pools in bounded store values.
5. **A tracked plugin logs `BeaconUpgraded` again**: its factory re-bound it to another beacon
   (`setBeacon` on Nami's plugins). `plugin_beacon` on that transaction gives the new beacon and
   implementation; move the pool to that beacon and update `plugin_impl`.

`map_protocol_changes` (`src/modules/6_map_protocol_changes.rs`) implements these five cases.
Two stores carry the beacon links (`src/modules/5_store_plugin_beacons.rs`):
`store_plugin_beacons` holds the beacon each plugin is bound to, and `store_beacon_pools` lists
the pools per beacon, spread over 256 keys because a store value is bounded. A pool stays listed
after its plugin moves to another beacon, so an upgrade is applied only to pools whose plugin is
still bound to the upgraded beacon.

`store_plugin_beacons` also records what each pool carries: the plugin whose attributes it has
(`followed:<pool>`, empty while the pool is paused or names no plugin) and whether that plugin's
implementation is known (`impl:<plugin>`). Every deletion, update and unpause is decided from
these two keys, read just before the event. Changes a plugin makes in a transaction that ends
with its pool on another plugin are not emitted.

A plugin re-bound to a beacon in a transaction that does not show it reading the beacon's
implementation loses its `plugin_impl`, and it stays without one until the pool names another
plugin: later upgrades of that beacon are applied only to pools that have a `plugin_impl`.

## What simulation does with the attributes

| Situation | Chains with the VM fallback (Ethereum) | Other chains |
|---|---|---|
| No plugin, or `BEFORE_SWAP` off | the pool's last fee | same |
| `plugin_impl` is registered | native handler | native handler |
| `plugin_impl` is unknown or missing | VM handler through DCI | the pool decodes and is not quoted |
| A field a module reads is missing | the pool decodes and is not quoted | same |
| A module meets a value it cannot model | the pool decodes and is not quoted | same |

A pool is never quoted with a guessed fee. Modules read every field strictly and do not default
a missing one, since every field is emitted when the pool names its plugin. A pool that is not
quoted stays in the stream: its snapshot decodes, every quote returns an error, and it is quoted
again once a delta brings its plugin back to something the simulation reproduces.

### What a quote of a Nami pool assumes

- **The block.** The fee moves with `block.timestamp`: by the minute around the NYSE open and
  close on a pool with a fee source, and by the second while a keeper's fee override fades. A
  quote is for the block it was made for.
- **The sender.** The plugin charges its MEVX executor a fee of 1. Quotes are for any other
  sender.
- **No backrun.** With a MEVX router set, `afterSwap` may swap the same pool again when it finds
  an arbitrage (about 0.5% of Base swap transactions). The swap's own output is as quoted, but
  the pool ends at another price than the state a quote returns, so a route that uses the pool
  twice prices its second hop from a moved state. The backrun's gas, up to the module's call gas
  budget of 5M at most, is not in the quote.

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
5. Add the module's name to the `modules` param of each manifest whose plugins contain it.
6. Bump the package version, release the DEX's spkg and resync that DEX. Deployments that do not
   list the module are unaffected.

## What a new DEX delivers

| Case | This package | Simulation |
|---|---|---|
| Its plugins use only indexed modules | a manifest per chain, listing them in `modules` | a registry entry per implementation |
| A new arrangement of indexed modules | nothing | a registry entry |
| A module of its own | a file under `src/plugin/modules/<dex>/`, a line in `MODULES`, its name in `modules`, a new spkg, a resync of that DEX | the module's file and the DEX's registry entries |
| It upgrades a beacon | nothing, if the layouts are unchanged | a registry entry for the new implementation, merged before the upgrade |

A registry entry for a new implementation is reviewed against its source: each module's namespace
constant and storage struct must match what `src/plugin/modules/` lists. A struct may gain fields
at its end; a moved slot or a reordered field needs a new table entry first.

Tests an adopter ships:

1. For each module, a storage word read from a deployed plugin decodes to the expected attributes.
2. Unit tests of each simulation module, edges included.
3. A differential check against a locally synced indexer: for every pool, the handler's quote
   equals `beforeSwap` called from the pool with `eth_call` at the same block, in both directions.
   The attributes come from the indexer, so the check covers the slot table and the simulation
   module together.
4. A range test covering at least one configuration change and one beacon `Upgraded`.

## Limits

- **Plugins that are not beacon proxies** get no `plugin_impl`. Algebra's earlier base plugins are
  deployed per pool with immutables, so an address does not identify their code.
- **Adaptive fee.** `dynamic_fee` indexes the fields of a flat fee. A non-zero alpha needs the
  volatility oracle's recent timepoints, which are not indexed; simulation rejects such a pool.
- **One layout per module name.** Module names are unique in `MODULES`. A deployment whose version
  of a module uses other slots writes nothing the table knows, and its pools would carry that
  module's fields as zero. Such a version is a second module entry under another name, listed in
  that deployment's `modules`.

## Deployment assumptions

- **Plugin created with its pool.** A plugin is created in the transaction that names it to its
  pool, as the plugin factory does when it creates both. Its configuration is then what it wrote
  in that transaction and zero elsewhere. For a plugin created earlier `named_plugin_attributes`
  returns `None`, and the pool modules pause the pool.
- **Beacon proxies.** Only a plugin constructed as a beacon proxy gets a `plugin_impl`. A pool
  without one is simulated through the VM where that is enabled and is not quoted elsewhere.

## Upgrade boundary

| Dependency | Followed how | What breaks |
|---|---|---|
| Plugin implementation behind a beacon | the beacon's `Upgraded` logs update `plugin_impl` on every pool whose plugin is bound to it | simulation rejects an implementation it has no registry entry for, so the pools stop being quoted until one is added |
| Module storage layout | fixed in `src/plugin/modules/` | a new implementation that moves a module's slots or repacks a word needs a new table entry, a new spkg and a resync; until then the old attributes go stale |
| The beacon a plugin is bound to | the plugin's later `BeaconUpgraded` logs, read with `plugin_beacon` | as for a beacon upgrade |
| A pool's plugin address | the pool's `Plugin` event | a plugin created before the transaction that names it pauses the pool |
| Contracts a plugin calls in `beforeSwap` (DCI chains, VM handler) | the trace of the `beforeSwap` entrypoint, registered when the pool enables the hook | the indexer does not retrace an entrypoint whose trace succeeded, so a plugin that starts calling another contract later (a fee source enabled after the first trace) fails its VM quotes with a missing account until the entrypoint is traced again |
| Nami's fee source contracts | not indexed: simulation ports them and looks them up by the address in `fee_override/source` | a new source contract needs a simulation entry before a pool is pointed at it |

A DEX opens the registry PR for a new implementation before it upgrades its beacon.
