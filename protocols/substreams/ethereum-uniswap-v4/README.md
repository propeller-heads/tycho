# Ethereum UniswapV4 Substreams

This directory contains refactored UniswapV4 substreams that filter Protocol Components based on swap hook presence.

## Structure

### Shared Library (`shared/`)
Contains common logic shared between both substreams:
- ABI definitions and protobuf types
- All processing modules except pool creation filtering
- Hook permissions detection utilities
- Mathematical utilities

### No-Hooks Variant (`no-hooks/`)
Tracks Protocol Components **WITHOUT** swap hooks:
- Filters out pools that have `beforeSwap` or `afterSwap` hook permissions
- Uses `HookPermissionsDetector::has_swap_hooks() == false`
- Configuration: `ethereum-uniswap-v4-no-hooks.yaml`

### With-Hooks Variant (`with-hooks/`)
Tracks Protocol Components **WITH** swap hooks:
- Only includes pools that have `beforeSwap` or `afterSwap` hook permissions  
- Uses `HookPermissionsDetector::has_swap_hooks() == true`
- Configuration: `ethereum-uniswap-v4-with-hooks.yaml`, `robinhood-uniswap-v4-with-hooks.yaml`

On Robinhood, `map_pons_enriched_block_changes` decodes a Pons V2 MemeHook pool's `hookFeeBps` and
`creatorTaxBps` straight from the storage writes its creation transaction's `registerPool` call
makes, rather than from an event or a stored attribute the hook exposes. Per-call storage writes
only reach a Substreams module on extended blocks, so a provider serving this manifest needs
`DetailLevel: EXTENDED`.

## Hook Permission Detection

Hook permissions are encoded in the least significant bits of the hook contract address:
- **Bit 7**: `beforeSwap` hook permission
- **Bit 6**: `afterSwap` hook permission

The `HookPermissionsDetector` utility extracts these flags and determines if a pool has swap hooks.

## Building

```bash
# Build all variants
cargo build --target wasm32-unknown-unknown --all-features

# Build specific variant
cd no-hooks/
substreams build

cd ../with-hooks/
substreams build
```

## Usage

### No-Hooks Substream
```bash
cd no-hooks/
substreams run ethereum-uniswap-v4-no-hooks.yaml map_protocol_changes --start-block 21688329
```

### With-Hooks Substream  
```bash
cd with-hooks/
substreams run ethereum-uniswap-v4-with-hooks.yaml map_protocol_changes --start-block 21688329
```

## Migration Notes

- The original unified substream configuration is preserved as `ethereum-uniswap-v4-original.yaml`
- Both variants maintain the same output format and module structure
- Only the pool creation filtering logic differs between variants
- ~95% code reuse through the shared library approach