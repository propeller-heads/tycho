# V4 mini router

`V4MiniRouter.evm.runtime` (Ethereum) and `UnichainV4MiniRouter.evm.runtime` (Unichain) are the
runtime bytecode of `V4MiniRouter.sol`. The hooks DCI installs this runtime by state override and
traces its sample swaps through it.

`V4MiniRouter.sol` is a copy of `evm/src/V4MiniRouter.sol` from
`propeller-heads/tycho-v4-hooks-prototype`, commit `f666a4634` ("feat: add receive ETH capability
to the minirouter"). Only its licence header is changed, to this repository's licence.

## Build

The runtimes come from this build of the prototype repository:

- solc 0.8.26, with the Foundry default settings of the prototype's `evm/foundry.toml`.
- Dependencies: v4-core v4.0.0 (`e50237c43811bd9b526eff40f26772152a42daba`) and v4-periphery
  `1ae55a683785f3f06a5f2bf6073fe0eda014b3eb`, as git submodules under `evm/lib`.
- `forge build --use 0.8.26 --contracts src/V4MiniRouter.sol` in `evm/`.

The PoolManager address is a constructor argument, so it is an immutable. The runtime is
`deployedBytecode.object` from `out/V4MiniRouter.sol/V4MiniRouter.json`, with the PoolManager
address written at each of its 11 `immutableReferences`:

| Runtime | PoolManager |
|---|---|
| `V4MiniRouter.evm.runtime` | `0x000000000004444c5dc75cB358380D2e3dE08A90` (Ethereum) |
| `UnichainV4MiniRouter.evm.runtime` | `0x1F98400000000000000000000000000000000004` (Unichain) |

This build gives the two runtimes byte for byte, except the last 32 bytes of the CBOR metadata
(the IPFS hash of the source metadata). The metadata does not change execution.
