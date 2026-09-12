# BaiBai on Base

Indexes the WETH/USDC pair of BaiBai CurveBook v3. The native simulator supports
exact-input swaps in both directions for zero-fee takers. The executor checks
that assumption for its router at execution time. Router-level fees are separate.

The component ID is `0x` followed by the concatenated 20-byte entrypoint and base token
addresses (lowercase hex). Static `base` and `quote`
attributes identify token roles independently of token ordering.

## State

`word_0` is the CurveBook v3 TTL. `word_1` through `word_5` are pair storage
slots 0 through 4 (sequences/validity, price/side settings, quantity unit, and
ask/bid cursors). `word_6` through `word_17` are the twelve packed ask-knot slots;
`word_18` through `word_29` are the bid slots. `word_30` and `word_31` are the
custodian's total claim reservations for base and quote. Words are unsigned,
big-endian integers, at most 32 bytes. Component balances are custody holdings;
simulation subtracts reservations before allowing an output transfer. The standard
`balance_owner` state attribute identifies the custodian. Native simulation does
not require indexed contract accounts.

All successful storage writes are tracked, including cursor consumption,
reanchors, shape replacements, TTL changes and claim settlement. The final
write to a slot is selected by execution ordinal, not nested call order.
Transfer logs track WETH/USDC holdings, including direct donations and withdrawals.
The simulator applies execution-block time even when there is no pool update.

## Building and running

From `protocols/substreams`:

```sh
cargo test -p base-baibai
cargo build -p base-baibai --target wasm32-unknown-unknown --release
substreams pack base-baibai/base-baibai.yaml
```

Run with an extended-block Base Substreams endpoint (storage changes are required).
Replay from the manifest's initial block. It is the entrypoint deployment block,
before custody and CurveBook storage exist. Storage starts at zero and is followed
through subsequent deployments and updates. Token balances are seeded using two
deterministic post-block `balanceOf` calls; transfer tracking starts in the next
block. Changing `start_block` to an arbitrary recent block is not a valid bootstrap.

The configuration selects one base/quote pair. Do not duplicate the shared
custodian's quote inventory across multiple routing components. Additional pairs
need explicit shared-inventory handling. Rebasing and fee-on-transfer tokens are
not supported. An incompatible CurveBook storage upgrade requires an indexer and
simulator update.

The executor deployment configuration selects the entrypoint. A production
executor deployment and Tycho indexing registration are required before routing
can be enabled. This contribution does not assign a production executor address.
