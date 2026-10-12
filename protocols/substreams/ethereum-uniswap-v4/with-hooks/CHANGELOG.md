# Changelog

## v0.7.1

- Pull in shared v0.5.3: join tick store deltas by key and ordinal, and aggregate repeated
  writes per transaction to preserve Creation and correctly restore existing ticks on reorg.
