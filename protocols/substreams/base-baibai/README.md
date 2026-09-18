# BaiBai on Base

Indexes the WETH/USDC pair of BaiBai CurveBook v3. The native simulator supports
exact-input swaps in both directions with the executing router's taker fee.
Set `DecoderContext::caller(router_address)` when registering `BaibaiState` through
`exchange_with_decoder_context`; use the same router address as the encoder.
An omitted caller defaults to Tycho's Base router. Other chains are rejected.
Router-level fees are separate.

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
Transfer, WETH Deposit and Withdrawal logs track custody holdings, including donations.
The simulator applies execution-block time even when there is no pool update.

Bid routing limits stop before the first remaining segment with nonpositive
marginal proceeds. Later segments beyond a flat or decreasing interval are not
advertised. Custody sizing uses a monotonic upper bound on rounded proceeds,
at most one quote-token atomic unit above the exact output, so every smaller
input stays within available custody. Individual quotes retain contract rounding.

The entrypoint's fee state is indexed from deployment. `pair_fee_<taker>` follows
fee events for this base token; `taker_fee_<taker>` follows events with base zero.
Precedence is the taker's pair override, then their taker-wide override, then zero.
Taker suffixes are 40 lowercase hex digits without `0x`. Override values are three
bytes: configured (0 or 1), then big-endian uint16 bps. Clearing writes `000000`;
configured zero is `010000` and overrides a nonzero taker-wide fee. The deployed
entrypoint has no venue-default fee storage field.

Each simulator retains only its caller's two overrides.
Updates take effect with indexed blocks, without per-quote RPC calls. The fee is
rounded up on gross output; the curve cursor consumes the gross fill while custody
pays only net output. Limits and prices include the fee. Execution uses the router's
existing minimum-output protection, so a fee change can cause normal slippage or a
revert. Changes to fee semantics or storage layout require an integration update.
Snapshots without `fees_indexed` require reindexing with this package.

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
not supported. The entrypoint, CurveBook and custodian are upgradeable proxies. Any `Upgraded`
event from these proxies after the validated block 51,191,196 marks the component
paused using Tycho's standard pause attribute. Historical upgrades through that
block are included in the validated replay. Pausing remains in effect until the
new implementations, immutable wiring and storage layout have been reviewed and
the package updated and reindexed; later ordinary updates do not resume routing.

The executor deployment configuration selects the entrypoint. A production
executor deployment and Tycho indexing registration are required before routing
can be enabled. This contribution does not assign a production executor address.
