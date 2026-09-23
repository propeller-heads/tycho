# Substreams Indexing Integrations

Please refer to the official [Substreams Indexing](https://docs.propellerheads.xyz/tycho/for-dexs/protocol-integration-sdk) docs.

## How to publish an spkg

Packages are built and published by the manual `release-substreams-package` job in the
[Release Substreams workflow](../../.github/workflows/release-substreams.yaml). Tagging alone
does not trigger anything — the workflow must be dispatched by hand.

### Release

1. Bump the package version in its `Cargo.toml` and add a `CHANGELOG.md` entry, merge to `main`.
2. Tag the merge commit with the Cargo package name and version, e.g.
   `git tag ethereum-curve-0.3.8 && git push origin ethereum-curve-0.3.8`. Nested packages use
   their Cargo package name too, so the tag never contains a slash:
   `ethereum-uniswap-v4-with-hooks-0.8.0`.
3. Dispatch the **Release Substreams** workflow with the tag as the ref, the package as the
   `package` input — either its Cargo package name (e.g. `ethereum-curve`) or its directory
   (e.g. `ethereum-uniswap-v4/with-hooks`), both resolve to the same release — and the manifest
   name without the `.yaml` extension (e.g. `ethereum-curve`) as the `config_file` input.

The build errors if the tag version does not match the package's `Cargo.toml` version. A release
tag for a different package, or one of the repository's own release tags, on the same commit is
ignored and produces a pre-release instead.

The spkg lands in the registry under
`substreams/<package directory>/<manifest name>-v<version>.spkg`. The namespace is the package
directory, so nested packages publish under their directory path, e.g.
`.../substreams/ethereum-uniswap-v4/with-hooks/robinhood-uniswap-v4-with-hooks-v0.8.0.spkg`.
Older objects in the registry use other namespaces — `publish.sh`, the manual publish script,
keys by manifest name instead — so always read the exact path the release prints rather than
guessing it.

Releases are immutable: the upload is an S3 conditional write that is rejected if the
spkg already exists in the registry. To ship new code, bump the package version — never
delete or re-point a release tag. Pre-releases are exempt and may be rebuilt. Running
`release.sh` locally requires AWS CLI >= 2.17 (S3 conditional write support).

### Pre-release

Dispatch the workflow from any branch or commit that is not exactly on a release tag.
This publishes `<manifest name>-pre.<short-sha>.spkg`, which you can use to test in dev.

### Packages with multiple manifests

The `config_file` input names the single manifest to pack, e.g. `ethereum-pancakeswap`
for `ethereum-uniswap-v2/ethereum-pancakeswap.yaml`. Packages that ship several manifests
(e.g. forked protocols) need one dispatch per manifest. When run locally without a
manifest argument, `release.sh` auto-discovers every manifest in the package directory
matching the chain name (or `substreams.yaml`) — CI keeps the input explicit for now.
Auto-discovery only matches the chain the package directory is named after, so manifests for
other chains (`robinhood-uniswap-v4-with-hooks.yaml` inside `ethereum-uniswap-v4/with-hooks`)
must be named explicitly.

### Dry run

`RELEASE_DRY_RUN=1 ./release.sh <package> <config_file>` resolves the package, validates the
tag and prints the build, pack and upload commands without running them. Use it to check the
object path before dispatching a release. `tests/release_sh_test.sh` exercises the resolution
rules this way against a throwaway copy of this directory.

### Reproducibility

Builds run with the package's own `rust-toolchain.toml` and the committed workspace
`Cargo.lock` (`--locked`), so rebuilding the same commit produces the same wasm. Every
package must pin an exact toolchain version — never `stable`. Note that any change to the
wasm produces a new substreams module hash, and the substreams servers rebuild the module
cache from the package's initial block on first sync.

## Test your implementation

To run a full end-to-end integration test you can refer to the [testing script documentation](../testing/README.md).
