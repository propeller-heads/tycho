# Algebra Integral

Shared indexing for DEXs built on Algebra Integral. One package serves every such DEX: each gets a
manifest per chain and keeps its own protocol system (`nami_v4`, ...).

This package currently holds the plugin part only: the functions under `src/plugin/` and their
tests. The pool modules and the manifests arrive with the first DEX that moves onto it, and the
sections below say what those modules have to do with these functions.

## Plugins

An Algebra pool with the `BEFORE_SWAP` flag asks its plugin for the fee of every swap. A plugin is
an `AlgebraPluginProxy`, a beacon proxy, and its implementation is a set of modules (dynamic fee,
MEVX, farming, ...) that each keep their configuration at slots fixed by their code.

The package emits, on the pool's component:

| Attribute | Value | From |
|---|---|---|
| `plugin_impl` | 20-byte address of the implementation the plugin runs | the plugin's read of `beacon.implementation()` in its creation transaction, then the beacon's `Upgraded` events |
| `<module>/<field>` | the field's bytes: big-endian, exactly as wide as the field | the plugin's storage writes, decoded through the module slot table |

Simulation picks the plugin's modules by `(chain, plugin_impl)` and builds each module from its
`<module>/<field>` attributes (`crates/tycho-simulation/src/evm/protocol/algebra_integral/plugin/`).

When a pool names its plugin, every indexed field of each slot the plugin wrote in that transaction
is emitted as a creation, zero values included. Later, only fields whose bytes changed are emitted.
Slots no module lists, such as the volatility oracle's timepoints, emit nothing.

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

## Deployment assumptions

- **Initial configuration.** A plugin's initial configuration is written in the transaction that
  names it to its pool, as the plugin factory does when it creates both. A pool pointed at a plugin
  configured in an earlier transaction would miss those writes, so the pool modules have to pause it.
- **Beacon proxies.** Only a plugin constructed as a beacon proxy gets a `plugin_impl`. A pool
  without one is simulated through the VM where that is enabled and is not quoted elsewhere.

## Upgrade boundary

| Dependency | Followed how | What breaks |
|---|---|---|
| Plugin implementation behind a beacon | `beacon_upgrades` reports `Upgraded` logs; the pool modules have to update `plugin_impl` on every pool under that beacon | simulation rejects an implementation it has no registry entry for, so the pools stop being quoted until one is added |
| Module storage layout | fixed in `src/plugin/modules/` | a new implementation that moves a module's slots or repacks a word needs a new table entry, a new spkg and a resync; until then the old attributes go stale |
| A pool's plugin address | the pool's `Plugin` event | see the initial-configuration assumption |

A DEX opens the registry PR for a new implementation before it upgrades its beacon.
