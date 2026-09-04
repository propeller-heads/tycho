# protocols/substreams/

Substreams modules compiled to WASM that extract on-chain protocol state and emit protobuf
messages consumed by `tycho-indexer`. **Separate WASM workspace** — not in root
`[workspace.members]`. Build target: `wasm32-unknown-unknown`.

## Layout

One directory per package, named `{chain}-{protocol}` after the chain it was first written for
(e.g. `ethereum-uniswap-v2`). A package is **not** one chain or one protocol: it ships **one
manifest per (chain, fork)** and the manifest name — not the directory — names the published
artifact. `ethereum-uniswap-v2/` alone carries `arbitrum-uniswap-v2.yaml`,
`base-pancakeswap-v2.yaml`, `bsc-uniswap-v2.yaml`, `robinhood-uniswap-v2.yaml` and more.

Each package contains:

- `{chain}-{protocol}.yaml` — manifest: package metadata, protobuf imports, module graph, initial
  block. One per chain/fork served by the package
- `src/` — Rust map/store modules emitting `BlockChanges` / `EntityChanges` protobufs
- `integration_test.tycho.yaml` (plus `integration_test_{chain}_{protocol}.tycho.yaml` per extra
  manifest) — block range + assertions for `protocols/testing`
- `CHANGELOG.md` — **mandatory**; a release requires an entry
- `rust-toolchain.toml` — exact toolchain pin (never `stable`); release builds use it for
  reproducible wasm output

Shared code lives in `crates/`: `tycho-substreams` (the Tycho protobuf models and helpers every
package builds on) and `substreams-helper`.

## Adding a new protocol

Copy `ethereum-template-factory` (pool-factory pattern) or `ethereum-template-singleton` (single
contract) as a starting point. Implement the map modules, update the manifest, add an
`integration_test.tycho.yaml`, and register the package in `protocols/substreams/Cargo.toml`
`members`.

## Versioning and release

Every PR that touches a package **must** bump that package's version in its `Cargo.toml` and add a
`CHANGELOG.md` entry before merging to main — never merge changes without a version bump.

- **Patch bump** (e.g. `0.3.2` → `0.3.3`): bug fixes, small adjustments
- **Major bump** (e.g. `0.3.2` → `0.4.0`): significant changes, breaking output format

Releasing is manual and **tagging alone does nothing**:

1. Merge the version bump + changelog entry to main.
2. Tag the merge commit `{package}-{version}` (e.g. `ethereum-curve-0.3.3`).
3. Dispatch the `release-substreams-package` job in
   `.github/workflows/release-substreams.yaml` with that tag as the ref, the `package` input, and
   the `config_file` input naming the single manifest (without `.yaml`). Packages with several
   manifests need one dispatch per manifest.

Publishes are immutable — S3 conditional writes (`--if-none-match '*'`) reject an existing spkg, so
shipping new code always means a version bump. Pre-releases land as `<pkg>-pre.<short-sha>.spkg`.
See `protocols/substreams/Readme.md`.
