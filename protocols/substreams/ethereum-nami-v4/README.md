# ethereum-nami-v4

Substreams package for Nami V4, a fork of Algebra Integral 1.2 whose pools call a plugin before
and after each swap. `ethereum-nami-v4.yaml`, `base-nami-v4.yaml` and `robinhood-nami-v4.yaml`
are the same package pointed at each chain's factory. The protocol system is `nami_v4`.

## What the package emits

- Pools from the factory's `Pool` event, with `sqrt_price_x96`, `tick`, `liquidity`, `fee` (the
  pool's stored fee), `plugin` and `plugin_config`.
- `ticks/<index>`: every initialized tick with its net liquidity. A tick is created when its gross
  liquidity turns positive and deleted when it returns to zero, because an Algebra tick bounds
  swap steps while any position references it, even at zero net liquidity.
- Balances from the `Transfer` logs that move a pool's own tokens in or out of it. Pool events
  alone miss the community fee, which the pool pays its vault with a plain transfer.
- The plugin's settings as pool attributes: `mevx_router`, `min_gas_left`, `call_gas_budget` and
  `incentive`, from the events the plugin emits from its own address. A plugin switch resets them,
  except for what the new plugin set in the same block. A setting the new plugin emitted in an
  earlier block is lost.
- A Dynamic Contract Indexer entrypoint for the plugin's `beforeSwap`, as the pool calls it, on
  every `PluginConfig` that enables the hook and again on every swap, so a plugin whose trace
  failed before trading opened is traced again once it works. Block storage changes come with it.

## The plugin is upgradeable

Every plugin is a beacon proxy, and every beacon has been upgraded at least once (Ethereum at
block 26297415). Selectors the plugin does not know delegate to an extension contract. An upgrade
can change the fee logic or the storage the indexed entrypoint reads. The entrypoint is traced
again on every swap, which picks up a changed code path, and `crates/tycho-simulation` runs the
indexed `beforeSwap` bytecode as is.

## Known limitation: the plugin's fee source

The plugin's fee-override module can blend the fee from a separate fee-source contract once an
admin sets one and a non-zero cap. The cap sits in a packed slot that holds no address, and the
indexer retraces an entrypoint on its own only when a slot holding a traced address changes, while
the per-swap re-registration of an already traced entrypoint is a cached success. Enabling the
fee source on a pool therefore leaves that contract unindexed, and the pool's quotes fail, until
the indexer restarts or the plugin's beacon is upgraded. Today no deployed plugin has a fee
source set.

## Testing

```bash
cargo test -p ethereum-nami-v4
cargo build -p ethereum-nami-v4 --target wasm32-unknown-unknown
```

The three `integration_test*.tycho.yaml` configs run on the `protocols/testing` runner. The
Ethereum range covers the plugin-settings events and a beacon upgrade.
