// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/**
 * @dev Minimal, ABI-compatible declarations of the PancakeSwap Infinity contracts the executor
 * calls. Hand-written from pancakeswap/infinity-core (commit d0e87933) instead of vendoring the
 * repo as a submodule, following the precedent of IFluidV1Dex.sol and IPropAMM.sol.
 *
 * Sources:
 * - PoolKey, six 32-byte words. `poolManager` is part of the key and of the PoolId.
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/types/PoolKey.sol#L12-L25
 * - Vault. `lock` calls back `ILockCallback.lockAcquired(bytes)` on msg.sender. `sync` is not
 *   lock-gated; `settle` and `take` are.
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/interfaces/IVault.sol#L56
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/Vault.sol#L74-L86
 * - CLPoolManager.swap.
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-cl/interfaces/ICLPoolManager.sol#L150-L166
 * - BinPoolManager.swap.
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/interfaces/IBinPoolManager.sol#L191-L193
 * - BalanceDelta, an int256 packing amount0 in the upper 128 bits and amount1 in the lower.
 *   https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/types/BalanceDelta.sol#L6-L18
 */
struct InfinityPoolKey {
    address currency0;
    address currency1;
    address hooks;
    address poolManager;
    uint24 fee;
    bytes32 parameters;
}

interface IPancakeswapInfinityVault {
    function lock(bytes calldata data) external returns (bytes memory);
    function sync(address currency) external;
    function settle() external payable returns (uint256 paid);
    function take(address currency, address to, uint256 amount) external;
    function currencyDelta(address settler, address currency)
        external
        view
        returns (int256);
}

interface IPancakeswapInfinityCLPoolManager {
    struct SwapParams {
        bool zeroForOne;
        int256 amountSpecified;
        uint160 sqrtPriceLimitX96;
    }

    function swap(
        InfinityPoolKey memory key,
        SwapParams memory params,
        bytes calldata hookData
    ) external returns (int256 delta);
}

interface IPancakeswapInfinityBinPoolManager {
    function swap(
        InfinityPoolKey memory key,
        bool swapForY,
        int128 amountSpecified,
        bytes calldata hookData
    ) external returns (int256 delta);
}
