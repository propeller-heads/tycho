# protocols/substreams/

Substreams modules compiled to WASM that extract on-chain protocol state and emit protobuf
messages consumed by `tycho-indexer`. **Separate WASM workspace** — not in root
`[workspace.members]`. Build target: `wasm32-unknown-unknown`.

## Layout

One directory per protocol: `{chain}-{protocol}` (e.g. `ethereum-uniswap-v2`). Each contains:

- `{name}.yaml` — manifest: package metadata, protobuf imports, module graph, initial block
- `src/` — Rust map/store modules emitting `BlockChanges` / `EntityChanges` protobufs
- `integration_test.tycho.yaml` — block range + assertions for `protocols/testing`
- `rust-toolchain.toml` — exact toolchain pin (never `stable`); release builds use it for
  reproducible wasm output

## Adding a new protocol

Copy `ethereum-template-factory` (pool-factory pattern) or `ethereum-template-singleton` (single
contract) as a starting point. Implement the map modules, update the manifest, add an
`integration_test.tycho.yaml`.

## Versioning

Every PR that touches a package **must** bump that package's version in its `Cargo.toml` and add a
`CHANGELOG.md` entry before merging to main — never merge package changes without both updates.

- **Minor bump** (e.g. `0.3.2` → `0.3.3`): bug fixes, small adjustments
- **Major bump** (e.g. `0.3.2` → `0.4.0`): significant changes, breaking output format
