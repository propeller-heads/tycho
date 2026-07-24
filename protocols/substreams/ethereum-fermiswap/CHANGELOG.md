# Changelog

## v0.1.2

- Switch `tycho-substreams` from a path dependency on the in-tree crate to the
  published `0.8.1`, so releases build against a fixed, published version. The
  package relies on the token balance retention fix (#1056) included in `0.8.1`:
  the trader vault contract change often carries only token balance updates,
  which `0.8.0` dropped as empty.
