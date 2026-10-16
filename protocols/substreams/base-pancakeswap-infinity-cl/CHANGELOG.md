# Changelog

## v0.1.0

Initial PancakeSwap Infinity concentrated-liquidity integration on Base (`CLPoolManager`
`0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b`, deployed at block 30544106). Indexes pools without
swap hooks and with a static LP fee, emitting the Uniswap v4 attribute schema so `UniswapV4State`
simulates them. Protocol fees are read from the pool's slot0 storage write at creation because the
`Initialize` event does not carry them.

Ships BNB and Robinhood manifests alongside the Base one. All three share the module graph and the
wasm binary; `CLPoolManager` and `Vault` are `map_pools_created` params because Robinhood is a
separate deployment (`0xeE04c68742e6Bf434bE8039580D2e89BBE55bc6f` and
`0x4F922d5B15e6691e0469663E4F5C4177f23c5FaF`, first block 56743018), while Base and BNB share
CREATE3 addresses.
