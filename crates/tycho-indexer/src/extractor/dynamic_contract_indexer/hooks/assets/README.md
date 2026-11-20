# V4 mini router

`V4MiniRouter.evm.runtime` (Ethereum) and `UnichainV4MiniRouter.evm.runtime` (Unichain) are the
runtime bytecode of `V4MiniRouter.sol`. The hooks DCI installs this runtime by state override and
traces its sample swaps through it.

## Build

The runtimes come from this Foundry build:

- solc 0.8.26, with the Foundry default compiler settings.
- Dependencies in `lib/`: v4-core v4.0.0 (`e50237c43811bd9b526eff40f26772152a42daba`), v4-periphery
  `1ae55a683785f3f06a5f2bf6073fe0eda014b3eb` and forge-std v1.9.6
  (`3b20d60d14b343ee4f908cb8079495c07f5e8981`).
- `V4MiniRouter.sol` in `src/`, then `forge build --use 0.8.26 --contracts src/V4MiniRouter.sol`.

The PoolManager address is a constructor argument, so it is an immutable. The runtime is
`deployedBytecode.object` from `out/V4MiniRouter.sol/V4MiniRouter.json`, with the PoolManager
address written at each of its 11 `immutableReferences`:

| Runtime | PoolManager |
|---|---|
| `V4MiniRouter.evm.runtime` | `0x000000000004444c5dc75cB358380D2e3dE08A90` (Ethereum) |
| `UnichainV4MiniRouter.evm.runtime` | `0x1F98400000000000000000000000000000000004` (Unichain) |

This build gives the two runtimes byte for byte, except the last 32 bytes of the CBOR metadata
(the IPFS hash of the source metadata). The metadata does not change execution.
