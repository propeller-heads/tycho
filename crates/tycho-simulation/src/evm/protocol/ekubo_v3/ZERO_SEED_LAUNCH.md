# Final zero-seed launch extension

Supported identity: Robinhood / 4663,
`0x517621Ea1bf51F31826819b118fa4dfEb4D198D2`, deployment block 106030313,
Core `0x00000000000014aA86C5d3c41765bb24e11bd701`.
Source: [evm-contracts 1d74ed414a5a91100440b128ef07fbe15acf37a5](https://github.com/EkuboProtocol/evm-contracts/tree/1d74ed414a5a91100440b128ef07fbe15acf37a5),
tree-equivalent to canonical main `fac9631e8fad77271f241733177060fc98be3545`.
Other launch addresses are not interpreted with this ABI.

The Robinhood extractor joins the final `LaunchCreated` event to Core's
initialization by pool ID within the transaction. `launch_config` contains four
big-endian u64s (tradingStart, decayRateX32, initialFee, finalFee), followed by two
big-endian i32s (installed lower/upper ticks). Missing or malformed configuration
fails decoding. Optional logo metadata is not fetched.

Snapshot pricing uses its header timestamp. `apply_block(BlockContext)` lets a
caller explicitly pin an execution timestamp, including rollback, without a
wall-clock fallback. Fee decay uses the contract's rounded tick ratio and
saturation order. The creator fee is rounded up on actual output for exact
input, or grosses up actual input for internal signed exact-output quotes.
Tycho's public API and executor expose exact input only. The simulator returns
partial-fill errors with the result; the executor's zero-debt settlement rejects
partial fills atomically.

The Robinhood executor forwards the ordinary 52-byte hop to the final extension;
the extension clamps the limit and overrides skip-ahead to 1. No encoding tail is
added. SDK >=3.4 supplies Core's compact-price rounding fix; this also corrects
one legacy MEVCapture test expectation. The contracts submodule pins the final
source for local execution tests; locally deployed SignedExclusiveSwap fixtures
now explicitly set its separate owner fee to zero.

## Verification and rollout

- Simulation tests include twelve sequential frozen-source trades across both
  orders/decimal combinations, five independent fee-curve digests, timestamp
  rollback, serialization, maximum fees, saturation and partial fills.
- The extractor tests the exact final Solidity event fixture, optional logo,
  deployment mismatch, truncated data and noncanonical integer encodings.
- `EkuboV3Launch.t.sol` exercises the provider hop/callback path against local
  final-source contracts, with amount, event and creator-fee-ledger reconciliation.

These checks do not establish live routing. Release the updated extractor and
reindex launch creation, release simulation, deploy/approve the updated executor
through the provider's normal process, and reconcile a pinned Robinhood fork
before activating routes. The runtime fixture is locally compiled test code,
not proof of deployed bytecode. The conservative 1,900,000 gas estimate still
needs provider-specific calibration. No deployment or release is part of this PR.
