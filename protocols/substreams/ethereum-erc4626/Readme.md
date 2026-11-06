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
     each a `signature` (e.g. `str()`) and its full `calldata` (hex, no `0x`). See below.
2. Generate the encoded params string:
   ```bash
   python params.py
   ```
3. Copy the output and replace both the `params.map_protocol_components` and the `params.map_protocol_changes` values in `substreams.yaml` (keep each quoted as a single string).
4. Rebuild the Substreams package if needed for deployment or testing.

## Entrypoints

At a vault's first `Deposit`/`Withdraw`, `map_protocol_changes` emits DCI entrypoints for
`totalSupply()`, `convertToShares`, `convertToAssets`, `maxDeposit` and `maxRedeem`, plus the
vault's `extra_entrypoints`. DCI indexes only the storage slots a trace reads, and that first
trace runs on whatever branch the vault was on at that block. A vault that reads part of its rate
only on some branches therefore needs an extra entrypoint that reads that state directly:

- `stUSDS`: `str()`. Its rate accrues `str` only when `block.timestamp > rho`, and every deposit
  sets `rho` to the current block, so the first trace never reads it.

The simulation reads each vault's redeem limit as `maxRedeem` for an owner holding the whole
supply, with the owner's balance written into the vault's balance mapping. The vault's balance
mapping must use one of the layouts listed in `SHARE_BALANCE_MAPPING_SLOTS`
(`crates/tycho-simulation/src/evm/protocol/erc4626/vm.rs`), or the component fails to decode.
