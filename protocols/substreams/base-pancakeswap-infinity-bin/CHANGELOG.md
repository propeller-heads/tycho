# Changelog

## v0.1.0

Initial PancakeSwap Infinity Liquidity Book integration (`BinPoolManager`
`0xC697d2898e0D09264376196696c51D7aBbbAA4a9`, deployed at block 30544163 on Base and 47214336 on
BNB). Indexes pools without swap hooks and with a static LP fee. Per-bin reserves are read from
BinPoolManager storage diffs because no event carries them, and the protocol fee is read from the
pool's slot0 storage write at creation because `Initialize` omits it.
