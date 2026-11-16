# Changelog

## v0.1.0

### Added

- Index DollarStore on Ethereum as `vm:dollarstore`: one component per hub-hub and hub-spoke pair,
  created when the pair's second asset is listed; the proxy's storage and token balances; the
  implementation and swap library through DCI entrypoints on `getSwapQuote`, and the price feeds
  through entrypoints on the feeds themselves.
