# Baseline Storage-Diff Quote State Design

## Context

PR <https://github.com/propeller-heads/tycho/pull/1076> currently refreshes Baseline quote state in the substream by calling `getQuoteState(address)` for every created or updated component. The maintainer concern is that an RPC call from substreams can hurt indexer performance and should be replaced with state derived from block data if possible.

Event-only reconstruction is not sufficient. The Baseline events in the current PR expose pool creation, swaps, and fee updates, but they do not expose the full quote state required by `tycho-simulation`. In particular, the simulation decoder currently requires a complete `BaselineQuoteState` on every delta and replaces the whole state during `delta_transition`.

The viable alternative is storage-diff reconstruction. This document captures the investigated storage layout, the quote-state reconstruction rules, and the implementation plan.

## Current Quote State Shape

The substream currently emits these 19 attributes from `getQuoteState`:

```text
snapshot_curve_blv
snapshot_curve_circ
snapshot_curve_supply
snapshot_curve_swap_fee
snapshot_curve_reserves
snapshot_curve_total_supply
snapshot_curve_convexity_exp
snapshot_curve_last_invariant
quote_block_buy_delta_circ
quote_block_sell_delta_circ
total_supply
total_b_tokens
total_reserves
reserve_decimals
liquidity_fee_pct
pending_surplus
should_settle_pending_surplus
max_sell_delta
snapshot_active_price
```

`crates/tycho-simulation/src/evm/protocol/baseline/decoder.rs` requires all of these fields. `crates/tycho-simulation/src/evm/protocol/baseline/state.rs` replaces the complete `BaselineQuoteState` on every protocol delta, so a storage-based substream should still emit complete snapshots, not sparse partial updates.

## Source Contracts

Relevant Baseline source files used for this storage-layout analysis:

```text
src/components/BLens.sol
src/components/BSwap.sol
src/components/BFactory.sol
src/libraries/StateLib.sol
src/libraries/MakerLib.sol
src/libraries/BlockPricingLib.sol
src/libraries/CurveLib.sol
src/libraries/FeeLib.sol
```

The deployed relay address used by the substream is:

```text
0xc81Fd894C0acE037d133aF4886550aC8133568E8
```

The relay routes `getQuoteState(address)` and other read methods to:

```text
0xA2EBEa6427C522852160ba2DE034e76Ac1301BeE
```

The storage still belongs to the relay, because the components run through the relay routing model.

## Storage Layout

Baseline uses namespaced storage. These are not ordinary low-numbered Solidity slots.

From `StateLib.sol`:

```solidity
uint256 internal constant POOL_SLOT =
    uint256(keccak256("Baseline.State.Pool")) - 1;

uint256 internal constant MAKER_SLOT =
    uint256(keccak256("Baseline.State.Maker")) - 1;

uint256 internal constant BLOCK_PRICING_SLOT =
    uint256(keccak256("Baseline.State.BlockPricing")) - 1;
```

For a bToken, each mapping base is:

```text
keccak256(abi.encode(bToken, namespace_slot))
```

Live Base validation for bToken `0x2A6b1BF66542CB1463541d211747B28C6bb39e83` confirmed that these formulas decode the on-chain `Pool`, `Maker`, and `BlockPricing` structs and match `getQuoteState`.

### Pool State

Namespace:

```text
keccak256("Baseline.State.Pool") - 1
```

Struct:

```solidity
struct Pool {
    ERC20 reserve;
    bool paused;
    uint256 totalSupply;
    uint128 totalReserves;
    uint128 totalBTokens;
    uint128 pendingSurplus;
    uint128 settledReserves;
    address feeRecipient;
    uint8 reserveDecimals;
    uint8 bTokenDecimals;
    address creator;
    uint64 creatorFeePct;
    uint64 protocolFeePct;
    uint64 liquidityFeePct;
    uint128 creatorClaimable;
    uint128 protocolClaimable;
    uint128 pendingYield;
}
```

Slot packing:

```text
base + 0: reserve address, paused bool
base + 1: totalSupply
base + 2: totalReserves uint128, totalBTokens uint128
base + 3: pendingSurplus uint128, settledReserves uint128
base + 4: feeRecipient address, reserveDecimals uint8, bTokenDecimals uint8
base + 5: creator address, creatorFeePct uint64
base + 6: protocolFeePct uint64, liquidityFeePct uint64, creatorClaimable uint128
base + 7: protocolClaimable uint128, pendingYield uint128
```

Only a subset of these fields is needed for quote state, but the full pool decode is useful for validation and future changes.

Note: some comments in `StateLib.sol` suggest slot boundaries, but the Solidity field declarations and compiler packing rules are the source of truth. Because `creator` is an address, `creatorFeePct` packs into the remaining bytes of `base + 5`; it does not start `base + 6`.

### Maker State

Namespace:

```text
keccak256("Baseline.State.Maker") - 1
```

Struct:

```solidity
struct Maker {
    bool initialized;
    uint128 blvPrice;
    uint128 swapFee;
    uint128 maxCirc;
    uint128 maxReserves;
    uint128 convexityExp;
    uint256 lastInvariant;
}
```

Slot packing:

```text
base + 0: initialized bool, blvPrice uint128
base + 1: swapFee uint128, maxCirc uint128
base + 2: maxReserves uint128, convexityExp uint128
base + 3: lastInvariant
```

### Block Pricing State

Namespace:

```text
keccak256("Baseline.State.BlockPricing") - 1
```

Struct:

```solidity
struct BlockPricing {
    uint128 startReserves;
    uint128 startSupply;
    uint128 blockBuyDeltaCirc;
    uint128 blockSellDeltaCirc;
    uint256 startLastInvariant;
    uint64 blockNumber;
}
```

Slot packing:

```text
base + 0: startReserves uint128, startSupply uint128
base + 1: blockBuyDeltaCirc uint128, blockSellDeltaCirc uint128
base + 2: startLastInvariant
base + 3: blockNumber uint64
```

## Reconstruction Algorithm

The storage-diff implementation must reproduce `BLens.getQuoteState`, not merely copy storage values.

High-level target:

```solidity
function getQuoteState(BToken bToken) external view returns (QuoteState memory state) {
    state.snapshotCurveParams = MakerLib.getSnapshotCurveParams(bToken);

    State.BlockPricing storage pricing = State.blockPricing(bToken);
    if (pricing.blockNumber == uint64(block.number)) {
        state.quoteBlockBuyDeltaCirc = pricing.blockBuyDeltaCirc;
        state.quoteBlockSellDeltaCirc = pricing.blockSellDeltaCirc;
    }

    State.Pool storage pool = State.pool(bToken);
    state.totalSupply = pool.totalSupply;
    state.totalBTokens = pool.totalBTokens;
    state.totalReserves = pool.totalReserves;
    state.reserveDecimals = pool.reserveDecimals;
    state.liquidityFeePct = pool.liquidityFeePct;
    state.pendingSurplus = pool.pendingSurplus;
    state.shouldSettlePendingSurplus =
        pricing.blockNumber != 0 &&
        pricing.blockNumber != uint64(block.number) &&
        pool.pendingSurplus > 0;
    state.snapshotActivePrice = CurveLib.computeActivePrice(state.snapshotCurveParams);
    state.maxSellDelta = MakerLib.maxSellDelta(bToken);
}
```

### Stored Curve Params

Equivalent of `MakerLib._getStoredCurveParams`:

```text
BLV           = maker.blvPrice
circ          = normalizeWad(pool.totalSupply - pool.totalBTokens, pool.bTokenDecimals)
supply        = normalizeWad(pool.totalBTokens, pool.bTokenDecimals)
reserves      = normalizeWad(pool.totalReserves, pool.reserveDecimals)
totalSupply   = normalizeWad(pool.totalSupply, pool.bTokenDecimals)
convexityExp  = maker.convexityExp
swapFee       = maker.swapFee
lastInvariant = maker.lastInvariant
```

### Committed Curve Params

Equivalent of `MakerLib._getCommittedCurveParams`:

1. Start from stored curve params.
2. If block pricing has never been initialized, return stored params.
3. If `pricing.blockNumber == current_block_number`, return stored params.
4. If block pricing is stale:
   - If pool is in safety (`pool.totalBTokens >= pool.totalSupply * 0.95e18 / 1e18`), compute safety surplus and subtract it from `params.reserves` in memory.
   - Otherwise, if `pool.pendingSurplus > 0`, add normalized `pendingSurplus` to `params.reserves` in memory.

This preview matters because `getQuoteState` can report quote state before the next on-chain write commits stale block pricing.

### Snapshot Curve Params

Equivalent of `MakerLib._previewBlockPricing`:

1. Read `pricing = State.blockPricing(bToken)`.
2. Read `committed = _getCommittedCurveParams(bToken)`.
3. If `pricing.blockNumber == 0`, snapshot is `committed`.
4. If `pricing.blockNumber == current_block_number`, snapshot is `BlockPricingLib.applyPoolSnapshot(committed, pricing)` and quote block deltas are exposed.
5. If pricing is stale and has pending deltas, snapshot is `BlockPricingLib.curveParamsFromDeferredState(committed, previewDeferredMakerState(...))`.
6. If pricing is stale with no pending deltas, snapshot is `committed`.

`applyPoolSnapshot` maps:

```text
BLV           = committed.BLV
swapFee       = committed.swapFee
convexityExp  = committed.convexityExp
lastInvariant = pricing.startLastInvariant
totalSupply   = committed.totalSupply
reserves      = pricing.startReserves
supply        = pricing.startSupply
circ          = committed.totalSupply - pricing.startSupply
```

### Quote Block Deltas

`quote_block_buy_delta_circ` and `quote_block_sell_delta_circ` must be emitted only when:

```text
pricing.blockNumber == current_block_number
```

Otherwise both values must be zero, even if the stored accumulator slots are nonzero. This was validated against Base:

```text
block 46809778: quote_block_buy_delta_circ nonzero
block 46809779: quote_block_buy_delta_circ resets to zero in getQuoteState while snapshot rolls forward
```

This is the main reason storage decoding must implement `BLens` semantics rather than blindly mapping slots to attributes.

### Max Sell Delta

Equivalent of `BlockPricingLib.maxSellDelta`:

```text
snapshotCirc = denormalizeWad(snapshot.circ, pool.bTokenDecimals)
alreadySold = quoteContext.blockSellDeltaCirc
sameBlockBuys = quoteContext.blockBuyDeltaCirc

if alreadySold >= snapshotCirc:
    maxSellDelta = 0
else if sameBlockBuys >= snapshotCirc - alreadySold:
    maxSellDelta = 0
else:
    maxSellDelta = snapshotCirc - alreadySold - sameBlockBuys
```

### Snapshot Active Price

Equivalent of `CurveLib.computeActivePrice`:

```text
if snapshot.circ == 0:
    price = snapshot.BLV
else:
    premium = fullMulDiv(
        snapshot.reserves - snapshot.BLV.mulWad(snapshot.circ),
        snapshot.convexityExp.mulWad(snapshot.totalSupply),
        snapshot.supply.mulWad(snapshot.circ)
    )
    price = snapshot.BLV + premium
```

The Rust implementation should reuse or mirror the existing Baseline math code where possible to avoid rounding drift.

## Substreams Architecture

Recommended implementation shape:

1. Add a Baseline storage module that decodes storage changes for relay-address changes.
2. Add durable stores for per-bToken `Pool`, `Maker`, and `BlockPricing` state.
3. On pool creation, initialize the stores from the creation transaction storage changes and creation event data.
4. On every relevant storage change, update the per-bToken store.
5. In `map_protocol_changes`, for each changed bToken, reconstruct full quote-state attributes from the stores and current block number.
6. Remove `eth_call getQuoteState` from the steady-state update path.

This follows the same basic pattern as existing storage-backed protocol integrations, but with full-state reconstruction layered on top.

## Identifying Changed Components

A storage-diff implementation still needs to know which bToken a changed slot belongs to.

Because the storage slot key is:

```text
keccak256(abi.encode(bToken, namespace_slot)) + offset
```

the substream can build reverse indexes for known bTokens:

```text
pool_slot_base -> bToken
maker_slot_base -> bToken
block_pricing_slot_base -> bToken
```

When a storage change arrives for the relay, check whether the changed key falls within the relevant slot windows:

```text
Pool:         base + 0 through base + 7
Maker:        base + 0 through base + 3
BlockPricing: base + 0 through base + 3
```

If so, decode the changed slot, update the store, and mark that bToken as updated for the transaction.

This avoids needing to invert keccak from arbitrary slots.

## Storage Changes Availability

Tycho already uses storage changes in other substreams. `tycho_substreams::block_storage::get_block_storage_changes` requires the extended block model, and Uniswap V3 decodes `call.storage_changes` directly from transaction traces.

The Baseline substream now uses two stores:

```text
store_slot_index:
    key:   slot:<32-byte storage slot>
    value: <bToken>|<state area>|<offset>

store_state_slots:
    key:   state:<bToken>:<state area>:<offset>
    value: <32-byte storage value>
```

`store_slot_index` is populated from created components. It precomputes the `Pool`,
`Maker`, and `BlockPricing` slots for each bToken because a raw storage slot cannot be inverted back
to the bToken mapping key.

`store_state_slots` iterates non-reverted transaction calls, filters `call.storage_changes`
to the singleton relay address, looks up each changed slot in `store_slot_index`, and writes
the latest 32-byte slot value with the storage-change ordinal.

`map_protocol_changes` now marks components updated from these state-slot deltas and reconstructs the
complete 19-field quote state from `store_state_slots`. The steady-state production path no
longer calls `getQuoteState`.

For storage-driven changes, `map_protocol_changes` reads `store_state_slots` at the latest
storage-change ordinal for the component. This is required because Substreams `get_last` reads the
beginning-of-block state, while same-block storage deltas need the post-change state at the relevant
ordinal.

Reconstruction treats a missing tracked state-slot key as a zero storage word, matching EVM storage
semantics. This is important because Substreams storage changes are sparse: an untouched zero slot is
not expected to appear in `storage_changes`, but it is still a valid input to the Baseline struct
decoder. Malformed stored slot values still fail reconstruction.

The stale block-pricing branch with pending buy/sell deltas is handled by the Rust port of
`MakerLib._previewDeferredMakerState`, including convexity relaxation and safety-surplus preview
logic. Fixture tests compare this branch against a mainnet `getQuoteState` oracle.

Public Base RPC does not expose `debug_traceTransaction`, `trace_replayTransaction`, or `debug_storageRangeAt`, but that does not block substreams if the Substreams provider supplies extended blocks with storage changes.

## Validation Plan

Use `getQuoteState` as the oracle during development and fixture generation. It must not be called
from the production substream handler.

Recommended tests:

1. Unit-test slot math:
   - namespace slot calculation
   - mapping base calculation
   - packed field decode for Pool, Maker, and BlockPricing

2. Unit-test quote-state reconstruction:
   - initial pool creation
   - same-block swap where quote deltas are exposed
   - next-block stale pricing where quote deltas are zero and snapshot rolls forward
   - pending surplus settlement behavior
   - safety surplus behavior

3. Integration-test against known Base blocks:
   - creation block `46809753` for bToken `0x2A6b1BF66542CB1463541d211747B28C6bb39e83`
   - swap block `46809778`
   - next block `46809779`
   - swap block `46809812`
   - next block `46809813`

4. During development only, compare reconstructed attributes against `getQuoteState` at the same block.

## Risks And Open Questions

### Rounding Drift

`getQuoteState` depends on Solidity fixed-point behavior from Solady and Baseline math. The Rust version must match rounding exactly. Reuse existing Rust Baseline math helpers where possible, and add fixture tests around known blocks.

### Private Contract Source

The Tycho PR should not require maintainers to access private source to understand the slot layout. The substream code should document the slot constants and struct packing clearly.

### Contract Upgrades

The relay is routed/modular. If Baseline upgrades storage layout or quote-state semantics, the storage decoder must be versioned or guarded. The current deployed Base/Ethereum relay uses the Baseline layout described here.

### Full-State Store Required

Storage diffs only show changed slots. Since simulation needs complete state, the substream must keep durable per-bToken state in stores. Emitting only changed attributes is not compatible with the current decoder.

### Same-Block Semantics

`quote_block_*_delta_circ` is block-scoped. It is not safe to use stale accumulator storage after the block changes. This must be encoded explicitly.

## Maintainer-Facing Summary

Event reconstruction is a dead end because events do not include enough state to reproduce `BLens.getQuoteState`.

Storage diffs are feasible. Baseline stores the required state in namespaced mappings keyed by bToken. We can decode `Pool`, `Maker`, and `BlockPricing` storage from substream storage changes, maintain full per-bToken stores, and reconstruct the same quote state as `BLens.getQuoteState` without per-component RPC calls.

The implementation is nontrivial because `getQuoteState` includes preview logic for stale block-pricing state and same-block accumulators. The production substream now reconstructs those values from storage; `getQuoteState` remains useful only as a test/oracle source when adding new fixtures.
