# Changelog

## v0.1.0

- Initial release: Camelot V3 (Algebra V1.9) indexing on Arbitrum One as a native integration
  (`camelot_v3`). Factory `0x1a3c9B1d2F0529D97f2afC5136Cc23e58f1FD35B`, created at block
  `101163738`; first pool at block `101213149`. Pools emit `globalState`, `liquidity`, the ticks
  positions reference, their `DataStorageOperator`'s fee configurations and its timepoint ring
  as state attributes, with the ring pruned to the entries the adaptive fee can still read.
- Toolchain pinned to Rust 1.96.0, the workspace root's pin, rather than the 1.83.0 most older
  packages still carry. `release.sh` builds inside the package directory, so this pin produces
  the released wasm.
