# Changelog

## v0.1.0

Initial PancakeSwap Infinity concentrated-liquidity integration on Base (`CLPoolManager`
`0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b`, deployed at block 30544106). Indexes pools without
swap hooks and with a static LP fee, emitting the Uniswap v4 attribute schema so `UniswapV4State`
simulates them. Protocol fees are read from the pool's slot0 storage write at creation because the
`Initialize` event does not carry them.

Ships a BNB manifest alongside the Base one. Infinity is deployed at the same addresses on both
chains, so the two manifests share the module graph and the wasm binary and differ only in the
package name and the start block.
