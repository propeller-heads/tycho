# Changelog

## 0.1.0

- Index Fluid DEX v2 D3 and D4 pools on Ethereum from the proxy deployment (block 26243693).
- Track pool state (ticks, `dexVariables`, `dexVariables2`, token reserves) from the storage
  writes of each pool event, including D4 borrows and paybacks.
- Register `getTokenBaseState` on the DEX v2 resolver for both tokens of every pool with the
  Dynamic Contract Indexer, so the dex base state and Fluid Liquidity are indexed. Every
  block's storage changes are emitted so the indexer keeps those contracts up to date.
- Report the native token as the zero address.
