# Changelog

## v0.1.3

- Switch `tycho-substreams` from a path dependency on the in-tree crate to the
  published `0.8.1`, so releases build against a fixed, published version.

## v0.1.2

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.

## v0.1.1

- Emit reserve-based token balances.

## v0.1.0

- Add the LunarBase protocol integration.
