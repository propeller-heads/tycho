# Ethereum ERC4626 Substreams

This Substreams package tracks ERC4626 vaults on Ethereum, emitting `ProtocolComponent`s for each vault and balance deltas for the underlying asset and share token based on standard `Deposit` and `Withdraw` events.

- **Supported shape:** Any vault that follows the ERC4626 event and value semantics (event `assets` equals the underlying asset amount, event `shares` equals the share token amount).
- **Known pools:** See `params.json` for the configured vaults (e.g., `spUSDC`, `spUSDT`, `spETH`, `sUSDS`, `sUSDC`, `sDAI`).
- **Exception:** Aave `StataToken` (stata/stk tokens) is not supported because its `Deposit`/`Withdraw` events can surface wrapped Aave aToken amounts instead of ERC4626 asset/share amounts, which breaks the standard assumptions used by this stream. Ethena is also excluded because its `redeem` is currently disabled (cooldown requirement), so it does not follow standard ERC4626 behavior.

## Adding an ERC4626 pool

1. Add a new object to `params.json` following the existing format (include the creation block):
   - `name`: human-readable vault name.
   - `address`: ERC4626 vault address (lowercase, no `0x`).
   - `tx_hash`: transaction hash that created/initialized the vault; used to emit the component in that block.
   - `block`: block number where the vault is created/initialized.
   - `asset`: underlying asset address (lowercase, no `0x`).
2. Generate the encoded params string:
   ```bash
   python params.py
   ```
3. Copy the output and replace the `params.map_protocol_components` value in `ethereum-erc4626.yaml` (keep it quoted as a single string).
4. Rebuild the Substreams package if needed for deployment or testing.
