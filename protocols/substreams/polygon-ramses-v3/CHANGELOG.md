# Changelog

## v0.1.1

- Add Robinhood Chain support via a second manifest, `robinhood-ramses-v3.yaml` (factory
  `0xE0c4ceb92d08CA985bB70fe0a22fEb121A9854A8`, first pool at block `16079383`). The wasm is reused
  unchanged: the Robinhood deployment emits the same events as the Polygon deployment this package
  was built for, including the Ramses-specific `Mint` with its extra `uint256 index` parameter, and
  keys pools by tick spacing with a governance-mutable fee.

  The version bump is required to release the manifest at all: `release.sh` takes the version from
  a `polygon-ramses-v3-<semver>` git tag and `polygon-ramses-v3-0.1.0` already exists, pointing at a
  commit that predates this manifest.

## v0.1.0

- Initial release: Ramses V3 indexing on Polygon (factory
  `0x2Bef16A0081565E72100D73CBe19B1Bd2d802380`).
