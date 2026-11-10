# Ethereum ERC4626 Substreams

This Substreams package tracks ERC4626 vaults on Ethereum, emitting `ProtocolComponent`s for each vault and balance deltas for the underlying asset and share token based on standard `Deposit` and `Withdraw` events.

- **Supported shape:** Any vault that follows the ERC4626 event and value semantics (event `assets` equals the underlying asset amount, event `shares` equals the share token amount).
- **Known pools:** See `params.json` for the configured vaults (e.g., `spUSDC`, `spUSDT`, `spETH`, `sUSDS`, `sUSDC`, `sDAI`, `stUSDS`).
- **Exception:** Aave `StataToken` (stata/stk tokens) is not supported because its `Deposit`/`Withdraw` events can surface wrapped Aave aToken amounts instead of ERC4626 asset/share amounts, which breaks the standard assumptions used by this stream. Ethena is also excluded because its `redeem` is currently disabled (cooldown requirement), so it does not follow standard ERC4626 behavior.

## Adding an ERC4626 pool

1. Add a new object to `params.json` following the existing format (include the creation block):
   - `name`: human-readable vault name.
   - `address`: ERC4626 vault address (lowercase, no `0x`).
   - `tx_hash`: transaction hash that created/initialized the vault; used to emit the component in that block.
   - `block`: block number where the vault is created/initialized.
   - `asset`: underlying asset address (lowercase, no `0x`).
   - `extra_entrypoints` (optional): DCI entrypoints traced in addition to the standard ones,
     each a `signature` (e.g. `str()`) and its full `calldata` (hex, no `0x`). See
     [Entrypoints](#entrypoints).
2. Generate the encoded params string:
   ```bash
   python params.py
   ```
3. Copy the output and replace the `params.map_protocol_components` value in `substreams.yaml`
   (keep it quoted as a single string, after the `&vaults` anchor). `map_protocol_changes` reads
   the same string through `*vaults`.
4. Rebuild the Substreams package if needed for deployment or testing.

## Entrypoints

At a vault's first `Deposit`/`Withdraw`, `map_protocol_changes` emits the standard ERC4626 DCI
entrypoints (`src/modules.rs`) plus the vault's `extra_entrypoints`. DCI indexes only the storage
slots of the vault a trace reads, and that first trace runs on whatever branch the vault was on at
that block. A vault that reads part of its rate only on some branches therefore needs an extra
entrypoint that reads that state directly:

- `stUSDS`: `str()`. Its rate reads `str` only when `block.timestamp > rho`, and every deposit and
  withdrawal sets `rho` to the block's timestamp, so the first trace never reads it.

Extra entrypoints are emitted only at a vault's first interaction, so adding one to an indexed
vault takes effect only after a resync from that vault's first interaction.

The simulation reads each vault's redeem limit as `maxRedeem` for an owner holding the whole
supply, with the owner's balance written into the vault's balance mapping. `balanceOf` must read
that balance unchanged from a Solidity mapping declared at one of the base slots in
`SHARE_BALANCE_MAPPING_SLOTS` (`crates/tycho-simulation/src/evm/protocol/erc4626/vm.rs`), or the
component fails to decode. A Vyper vault, or one that packs or scales its balances, needs a
different lookup, not another slot. `get_amount_out` refuses a deposit above `maxDeposit` or a
redeem above `maxRedeem`.

## Deployment

stUSDS's `maxRedeem` reads `vat.ilks(ilk)` from the MakerDAO Vat
(`0x35d1b3f3d7966a1dfe207aa4514c12a259a0492b`), `jug.ilks(ilk)` from the Jug
(`0x19c0976f590d67707e62397c87829d896dc0f1f1`) and `Due()` from the LockstakeClipper
(`0x836f56750517b1528b5078cba4ac4b94fba4a399`). DCI indexes the full storage of a traced contract
that is not a token, unless it is listed in the indexer's `DCI_BLACKLIST_ADDRESSES`
(comma-separated). The Vat holds about 140k storage slots, so without the blacklist the erc4626
extractor stores and streams all of them to every client. Before deploying, add the Vat (and
optionally the Jug and the Clipper) to `DCI_BLACKLIST_ADDRESSES` of the indexer running the
erc4626 extractor; DCI then keeps only the slots the traces read. The variable applies to every
extractor of that indexer, so it must not list a contract another protocol needs in full.
