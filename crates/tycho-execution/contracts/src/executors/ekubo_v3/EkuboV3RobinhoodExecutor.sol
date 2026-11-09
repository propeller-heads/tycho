// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
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
address constant ZERO_SEED_LAUNCH_ADDRESS =
    0x517621Ea1bf51F31826819b118fa4dfEb4D198D2;

/// Ekubo V3 executor for the Robinhood deployment. Adds Ve33 and the final
/// zero-seed launch extension, forward-only call points with fixed 52-byte hop
/// encoding, on top of the chain-agnostic executor's extensions.
contract EkuboV3RobinhoodExecutor is EkuboV3Executor {
    constructor(address signedExclusiveSwap_)
        EkuboV3Executor(signedExclusiveSwap_)
    {}

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
        address extension = poolKey.config.extension();
        if (extension != VE33_ADDRESS && extension != ZERO_SEED_LAUNCH_ADDRESS)
        {
            return super._swapHop(poolKey, swapParameters, swapData, offset);
        }

        (balanceUpdate,) = abi.decode(
            CORE.forward(extension, abi.encode(poolKey, swapParameters)),
            (PoolBalanceUpdate, PoolState)
        );
        nextOffset = offset;
    }
}
