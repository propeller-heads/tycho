# Monad Kuru Substreams

Indexes Kuru order book markets on Monad into Tycho `BlockChanges` for the `kuru` protocol system.

## Components

One component per market registered by the Kuru Router (`params`: the Router address). The
component id is the market address, the tokens are the market's base and quote assets (native MON
is `0x0000000000000000000000000000000000000000`), and the protocol type is `kuru_market`.

Static attributes: `base`, `quote`, `price_precision`, `size_precision`, and `base_decimals` /
`quote_decimals` when known at registration.

## State

- Book levels: one attribute per price level, `b/<price>` for bids and `a/<price>` for asks
  (price in `pricePrecision` units), holding the total resting size. Market events drive them
  through the rules in the `kuru-book` crate, which `tycho-simulation` uses too.
- Vault quote, market state and fees: read from market storage (`vault_*`, `active`,
  `taker_fee_bps`, `maker_fee_bps`), because vault fills move storage without a complete event
  trail. This needs Extended blocks, which Monad serves.
- Balances: resting book depth, asks in base and bids in quote. They serve TVL only; the vault
  holds liquidity the book levels do not show.
