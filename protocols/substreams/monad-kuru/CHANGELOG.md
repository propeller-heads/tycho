# Changelog

## 0.1.1

- Filter `map_markets` and `map_events` with the `ethereum-common` v0.3.3 `index_events` block index
  (topic0 of `MarketRegistered` and of the six order book events). `map_protocol_changes` stays
  unfiltered because it reads market storage writes that may carry no market log.

## 0.1.0

- Kuru markets on Monad: levels from market events (shared `kuru-book` rules), vault/state/fees from market storage, book depth as balances.
