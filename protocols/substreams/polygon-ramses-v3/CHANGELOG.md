# Changelog

## v0.1.1

- Add Arbitrum support via a second manifest, `arbitrum-ramses-v3.yaml`. The wasm is
  reused unchanged: the Arbitrum deployment (factory
  `0xd0019e86edB35E1fedaaB03aED5c3c60f115d28b`, first pool at block `421077300`) emits
  events byte-identical to the Polygon deployment this package was built for, including
  the Ramses-specific `Mint` with its extra `uint256 index` parameter.
- Align `Cargo.toml` version (`0.1.0`) with the version already recorded in the
  workspace `Cargo.lock` (`0.1.1`); `--locked` release builds fail on the mismatch.

## v0.1.0

- Initial release: Ramses V3 indexing on Polygon (factory
  `0x2Bef16A0081565E72100D73CBe19B1Bd2d802380`).
