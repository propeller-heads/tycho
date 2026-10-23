# Changelog

## v0.1.0

Tessera V integration on Base as `vm:tessera`, indexed from block 37518600.

Each Pair is one component with its address as ID. A Pair is discovered when its EIP-1967
implementation, base/quote tokens and Engine registration are written in the same committed
transaction. TesseraSwap, the Engine and the Pair are stateful contracts. The Pair's
implementation, pricing library and fee module are published as
`stateless_contract_addr_0/1/2` and followed through their slot writes, each with its code as
`stateless_contract_code_0/1/2`, read with `eth_call`. The package must run without partial
blocks: an `eth_call` on a Flashblocks partial block stalls the stream.

Balances are held by the TesseraSwap treasury (slot 1), exposed as `balance_owner`. New
components are seeded with `balanceOf` at the end of the block, then follow ERC20 transfers and
canonical WETH deposits and withdrawals. On a treasury rotation, the block's events are counted
against the previous treasury and the difference between the two treasuries' closing balances
is applied at the rotation.

A component is marked `paused` when its fee module charges a nonzero tag-0 fee, and every
component is paused once TesseraSwap points to a different Engine. A Pair is also paused when
the Engine registry moves its token pair to another Pair or clears it, and the adapter refuses to
quote a Pair the registry does not point at.
