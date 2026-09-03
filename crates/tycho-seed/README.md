# tycho-seed

Shared pieces of the **seed writers**: native binaries that reconstruct a protocol's state at one
block and write it as a seed file for the protocol's Substreams package. A package that supports
seeding embeds that file and, at the seed block, emits the whole state as one synthetic
transaction, so its stores start from there instead of from the protocol's first block. Together
with a manifest whose `initialBlock` is the seed block, an indexer skips the protocol's history.

## Layout

| Where | What |
|---|---|
| `protocols/substreams/crates/tycho-seed-format` | The seed file header (magic, package name, block number, block hash) and the synthetic transaction hash. Dependency-free; built for `wasm32` by the packages and natively by the writers, so reader and writer cannot drift. |
| `protocols/substreams/crates/tycho-substreams`, module `seed` | The package side of the protocol-free work: parse the embedded seed for the package's own name, assert the streamed block, build the synthetic transaction. |
| `crates/tycho-seed` (this crate) | Seed files on disk, archive-node access (block headers, chunked `eth_getLogs`, full storage dumps), the manifest rewrite, and the subcommands every writer offers: `empty`, `inspect`, `manifest`. |
| `protocols/substreams/<protocol>/package/` | The Substreams package itself, with `src/genesis.rs` turning the seed body into the package's own snapshot events and the committed `seed.bin`: an empty body pinned to the block before the protocol's first, which the stock manifest never streams. |
| `protocols/substreams/<protocol>/seed/` | The writer, a member of the root workspace. Holds the reconstruction for each source it supports (`rpc` so far) and includes the package's generated protobuf code by path. |
| `protocols/substreams/pack-seeded.sh` | Builds any protocol's package with a given seed and packs the spkg with `initialBlock` moved to the seed block. |

A seeded protocol's directory therefore holds two crates side by side, `package/` (wasm32, with its
own `.cargo/config.toml` forcing that target) and `seed/` (native), plus the integration test
config. Keeping the writer out of the package directory is what lets plain cargo commands work in
both.

## Seed file

```text
tycho-seed-v1 | name length (u8) | package name | block number (u64 BE) | block hash (32) | body
```

The body is the package's own protobuf message; the format crate never interprets it. An empty
body is a valid seed of nothing. The package name is the Cargo name of the Substreams package, so a
seed written for one package is rejected by every other one at build time.

Seeded components carry `keccak256(magic ‖ package name ‖ block hash)` as their creation
transaction, computed by `Header::genesis_transaction_hash`.

## Conventions a seeded package follows

- The seed is the state **after** block N. At N the package emits only the synthetic transaction
  and drops the block's real events; its index follows the block's real transactions.
- The manifest streamed against a real seed has every `initialBlock` set to N (`manifest`
  subcommand, done by `pack-seeded.sh`). The extractor's `start_block` is exactly N.
- The package always embeds `seed.bin` and panics on an empty, truncated or foreign file. The
  committed file is the empty seed from `empty`, so the stock manifest's output is unchanged.
- The writer runs through cargo, never from `PATH`:

  ```sh
  cargo run --release -p ethereum-ekubo-v3-seed -- rpc --block 25895000 --out ekubo-v3-25895000.seed.bin
  cargo run --release -p ethereum-ekubo-v3-seed -- inspect ekubo-v3-25895000.seed.bin
  protocols/substreams/pack-seeded.sh protocols/substreams/ethereum-ekubo-v3 ekubo-v3-25895000.seed.bin
  ```

  `RPC_URL` (flag or environment, `.env` is loaded) must point at an archive node; the `rpc`
  source needs `debug_storageRangeAt`.

## Adding a writer for another package

1. Give the package a snapshot event and a `Seed` body message in its proto, and the arms that
   rebuild each store and map module from the snapshot.
2. Move the package into `<protocol>/package/` (the Substreams workspace member path, the
   integration test config's `substreams_yaml_path` and `release.sh` handle the nesting) and add
   `src/genesis.rs`: `tycho_substreams::seed::Seed::parse(SEED_BYTES, env!("CARGO_PKG_NAME"))`,
   `assert_block`, `transaction`, then the body into the package's events; gate `map_events` on the
   seed block.
3. Create `<protocol>/seed/` with `workspace = "../../../.."` in its manifest, list it in the root
   workspace, include the package's generated protobuf module by path, flatten
   `tycho_seed::cli::Common` into the CLI and add one subcommand per source.
4. Commit the empty seed from `empty` as `<protocol>/package/seed.bin`.

## Verifying a seed

The reference is any Tycho instance that indexed the protocol from its first block, typically the
existing dev or prod instance. Pick the seed block `N` far enough back that the window to `N + k`
exercises the protocol, sync a second instance from the seeded spkg with `start_block: N`, and
compare both instances' `/v1/protocol_state` and `/v1/protocol_components` at `N + k` per
component. `created_at` and `creation_tx` differ on purpose; what else may differ is up to the
package (see `scripts/compare-ekubo-seed.py` for the Ekubo v3 rules).
