# Changelog

## v0.2.2

- Track Ve33 pool swap-fee changes for accurate off-chain quotes. The Ve33
  extension address is chain-specific and passed to the package via module
  params; omitting it (e.g. on Ethereum) disables Ve33 handling.
- Align the `Cargo.toml` version with the `substreams.yaml` package version
  (previously out of sync).

## v0.1.2

- Tag SignedExclusiveSwap pools with the reserved `is_exclusive` static attribute
  (value `[1u8]`) at component creation, so consumers can keep pools requiring
  off-chain swap authorization out of public routing.

## v0.1.1

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
