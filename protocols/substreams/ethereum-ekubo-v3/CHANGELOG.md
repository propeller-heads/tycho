# Changelog

## v0.2.2

- Track the second TWAMM extension deployment
  (`0xd47f1b1edcfeabb08f6ebd8fc337c27e636c75ba`), first used in block 24995117.
  Both deployments are now recognized as TWAMM emitters and as extensions
  carrying time-rate deltas.
- Bump `Cargo.toml` from `0.1.1` to `0.2.2` so it matches the package version in
  `substreams.yaml`, which had drifted ahead.

## v0.1.1

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
