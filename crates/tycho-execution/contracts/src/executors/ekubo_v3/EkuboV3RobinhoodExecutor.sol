// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {EkuboV3Executor, CORE} from "./EkuboV3Executor.sol";
import {ICore} from "@ekubo-v3/interfaces/ICore.sol";
import {FlashAccountantLib} from "@ekubo-v3/libraries/FlashAccountantLib.sol";
import {PoolKey} from "@ekubo-v3/types/poolKey.sol";
import {PoolBalanceUpdate} from "@ekubo-v3/types/poolBalanceUpdate.sol";
import {PoolState} from "@ekubo-v3/types/poolState.sol";
import {SwapParameters} from "@ekubo-v3/types/swapParameters.sol";

using FlashAccountantLib for ICore;

address constant VE33_ADDRESS = 0xD18685a514E59b06d59824e16Db07e73345d9953;

/// Ekubo V3 executor for the Robinhood deployment. Adds the Ve33 extension,
/// a forward-only swap call point with the normal fixed 52-byte hop
/// encoding, on top of the chain-agnostic executor's extensions.
contract EkuboV3RobinhoodExecutor is EkuboV3Executor {
    function _swapHop(
        PoolKey memory poolKey,
        SwapParameters swapParameters,
        bytes calldata swapData,
        uint256 offset
    )
        internal
        virtual
        override
        returns (PoolBalanceUpdate balanceUpdate, uint256 nextOffset)
    {
        if (poolKey.config.extension() != VE33_ADDRESS) {
            return super._swapHop(poolKey, swapParameters, swapData, offset);
        }

        (balanceUpdate,) = abi.decode(
            // slither-disable-next-line calls-loop
            CORE.forward(VE33_ADDRESS, abi.encode(poolKey, swapParameters)),
            (PoolBalanceUpdate, PoolState)
        );
        nextOffset = offset;
    }
}
