# Changelog

## v0.1.1

Tessera V integration on Base as `vm:tessera`, indexed from block 37518600.

Each Pair is one component with its address as ID. A Pair is discovered when its EIP-1967
implementation, base/quote tokens and Engine registration are written in the same committed
transaction. TesseraSwap, the Engine and the Pair are stateful contracts. The Pair's
implementation, pricing library and write helper are published as
`stateless_contract_addr_0/1/2` and followed through their slot writes.

Balances are held by the TesseraSwap treasury (slot 1), exposed as `balance_owner`. New
components are seeded with `balanceOf` at the end of the block, then follow ERC20 transfers and
canonical WETH deposits and withdrawals. On a treasury rotation, the block's events are counted
against the previous treasury and the difference between the two treasuries' closing balances
is applied at the rotation.

A component is marked `paused` when its write helper charges a nonzero tag-0 fee, and every
component is paused once TesseraSwap points to a different Engine.
