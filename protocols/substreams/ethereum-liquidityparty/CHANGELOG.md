# Changelog

## v0.1.3

- Flag every pool token in the `self_contained_tokens` static attribute, as Fermiswap does. Without
  it, token proxies resolved to another protocol's stale token implementation and quotes buying
  PYUSD failed with `MissingAccount`. Static attributes are written only at pool creation, so a
  resync (re-index from the first pool creation block) is required.

## v0.1.2

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
