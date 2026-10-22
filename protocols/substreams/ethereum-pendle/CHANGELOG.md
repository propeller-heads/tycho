# Changelog

## v0.1.0

Initial Pendle V2 integration on Ethereum, from the original market factory's deployment (block
16032059) and covering all five factories. Emits two component types under one protocol system:
`pendle_market`, the SY↔PT AMM, and `pendle_sy`, the ERC-5115 wrapper behind it.

Market reserves are replayed from `Swap`, `Mint` and `Burn`, which are identical across factory
generations where the storage layout is not. Fees are read back from each factory's
`getMarketConfig`. Each SY's tokens are classified at creation as `one_to_one` or `index_rate` by
probing `previewDeposit` and `previewRedeem`, and re-classified on the SY's `Upgraded` and
`Unpaused`; a token matching neither is left out. Classes and `paused` are state, and each market
carries its SY's pause as `sy_paused`. An SY that caps its own supply carries the cap as
`sy_supply_cap`, and every SY whose `yieldToken()` answers its `yield_token`. An SY's supply and
balances are read once at creation and followed from events afterwards: balances of tokens kept in
shares (stETH, eUSD, STRCx, Aave aTokens) in shares, from `TransferShares` or the aToken's `Mint`,
`Burn` and `BalanceTransfer`, and valued at the ratio those events reveal; vault mints and burns that emit no `Transfer` (sDAI) from ERC-4626 `Deposit` and
`Withdraw`. Each entry token carries the step the SY mints in where the SY rounds its mint, and the
SYs whose deposits go through the sUSDat vault carry its entry fee and the time it applies from,
followed from the vault's events. `SY.exchangeRate()` is read
every `sy_rate_refresh_blocks` blocks (default 1), for the SYs of live markets and for SYs that have
ever had an `index_rate` token, and each market's PY index is derived from it and the stored index
its YT reports. A market reports a YT balance of zero on creation and on every reserve move, so that
Tycho counts YT as traded although no component holds it.
