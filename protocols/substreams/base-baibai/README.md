# BaiBai on Base

Discovers base-token pairs from successful `CurveUpdated` events on the configured
BaiBai CurveBook deployment. All pairs share its configured quote token. The native simulator supports
exact-input swaps in both directions with the executing router's taker fee.
Set `DecoderContext::caller(router_address)` when registering `BaibaiState` through
`exchange_with_decoder_context`; use the same router address as the encoder.
An omitted caller defaults to Tycho's Base router. Other chains are rejected.
Router-level fees are separate.

The component ID is `0x` followed by the concatenated 20-byte entrypoint and base token
addresses (lowercase hex). Static `base` and `quote`
attributes identify token roles independently of token ordering; static `custodian`
identifies the shared inventory owner.

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
Fee history is retained before pair discovery, so new components inherit both
applicable override levels. No fee-completeness sentinel is required.

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
through subsequent deployments and updates. Pairs are created on their first
successful curve publication, irrespective of sequence number or current depth.
An uninitialized v3 curve remains unquotable until a v3 SHAPE initializes it.

Custody is stored once per token and fanned out to every pair using that token;
TTL, quote reservations and taker-wide fees likewise update every affected pair.
The quote balance is bootstrapped at deployment. Each newly discovered base uses
one deterministic post-block `balanceOf` read, subtracting all of that block's
transfer deltas to recover its opening balance. This includes pre-listing deposits
without double-counting same-block transfers. Normal updates and quotes use no RPC.
Changing `start_block` to an arbitrary recent block is not a valid bootstrap.
Reindex from deployment when changing this unreleased package's module graph.

Routing multiple pairs requires shared custody **within each simulated route**.
After each swap, call `BaibaiState::sync_custody` on sibling states with the same
`custodian`, then recompute their limits. Keep each candidate's states separate.
Pair curves and fill cursors remain independent; balances and claim reservations
for intersecting tokens are shared. Independent per-pool limits must not be added
as though they represented separate inventories.

Rebasing and fee-on-transfer tokens are not supported. The entrypoint, CurveBook
and custodian are upgradeable proxies. Any `Upgraded` event from these proxies
after the validated block 51,696,183 marks existing and subsequently discovered
components paused using Tycho's standard pause attribute. Historical upgrades
through that block are included in replay. Client-side removal of paused components is an upstream `tycho-client` follow-up;
this adapter emits the standard attribute and does not duplicate that mechanism.
Routing across future upgrades requires that client support. Review
new implementations, immutable wiring and storage layout before updating the
package and reindexing.

The executor deployment configuration selects the entrypoint. A production
executor deployment and Tycho indexing registration are required before routing
can be enabled. This contribution does not assign a production executor address.
