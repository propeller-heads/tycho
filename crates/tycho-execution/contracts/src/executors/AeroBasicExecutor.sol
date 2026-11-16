// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IAeroBasicPool} from "@interfaces/IAeroBasicPool.sol";
import {IAerodromeV1Pool} from "@interfaces/IAerodromeV1Pool.sol";
import {AerodromeV1Executor} from "./AerodromeV1Executor.sol";

/// @notice Swaps on AERO basic (stable and volatile) pools, which take Aerodrome V1 calldata.
/// The output is priced with `getAmountOutWithTotalFee`: a pool whose factory charges a MEV tax
/// on this transaction reverts when asked for the `getAmountOut` amount, which leaves the tax out.
contract AeroBasicExecutor is AerodromeV1Executor {
    function _getAmountOut(
        IAerodromeV1Pool pool,
        uint256 amountIn,
        address tokenIn
    ) internal view override returns (uint256) {
        return IAeroBasicPool(address(pool))
            .getAmountOutWithTotalFee(amountIn, tokenIn);
    }
}
