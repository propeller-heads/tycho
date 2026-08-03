# Substreams Indexing Integrations

Please refer to the official [Substreams Indexing](https://docs.propellerheads.xyz/tycho/for-dexs/protocol-integration-sdk) docs.

## How to publish an spkg

Packages are built and published by the manual `release-substreams-package` job in the
[Release Substreams workflow](../../.github/workflows/release-substreams.yaml). Tagging alone
does not trigger anything — the workflow must be dispatched by hand.

### Release

1. Bump the package version in its `Cargo.toml` and add a `CHANGELOG.md` entry, merge to `main`.
2. Tag the merge commit with the package name and version, e.g.
   `git tag ethereum-curve-0.3.8 && git push origin ethereum-curve-0.3.8`.
3. Dispatch the **Release Substreams** workflow with the tag as the ref, the package
   name (e.g. `ethereum-curve`) as the `package` input, and the manifest name without
   the `.yaml` extension (e.g. `ethereum-curve`) as the `config_file` input.

The build errors if the tag version does not match the package's `Cargo.toml` version.
The spkg lands at `s3://repo.propellerheads-propellerheads/substreams/<package>/<package>-v<version>.spkg`.

Releases are immutable: the upload is an S3 conditional write that is rejected if the
spkg already exists in the registry. To ship new code, bump the package version — never
delete or re-point a release tag. Pre-releases are exempt and may be rebuilt. Running
`release.sh` locally requires AWS CLI >= 2.17 (S3 conditional write support).

### Pre-release

Dispatch the workflow from any branch or commit that is not exactly on a release tag.
This publishes `<package>-pre.<short-sha>.spkg`, which you can use to test in dev.

### Packages with multiple manifests

The `config_file` input names the single manifest to pack, e.g. `ethereum-pancakeswap`
for `ethereum-uniswap-v2/ethereum-pancakeswap.yaml`. Packages that ship several manifests
(e.g. forked protocols) need one dispatch per manifest. When run locally without a
manifest argument, `release.sh` auto-discovers every manifest in the package directory
matching the chain name (or `substreams.yaml`) — CI keeps the input explicit for now.

### Reproducibility

Builds run with the package's own `rust-toolchain.toml` and the committed workspace
`Cargo.lock` (`--locked`), so rebuilding the same commit produces the same wasm. Every
package must pin an exact toolchain version — never `stable`. Note that any change to the
wasm produces a new substreams module hash, and the substreams servers rebuild the module
cache from the package's initial block on first sync.

## Test your implementation

To run a full end-to-end integration test you can refer to the [testing script documentation](../testing/README.md).
