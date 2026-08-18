// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {EkuboV3ExecutorBase, CORE} from "./EkuboV3ExecutorBase.sol";
import {ICore} from "@ekubo-v3/interfaces/ICore.sol";
import {FlashAccountantLib} from "@ekubo-v3/libraries/FlashAccountantLib.sol";
import {PoolKey} from "@ekubo-v3/types/poolKey.sol";
import {PoolConfig} from "@ekubo-v3/types/poolConfig.sol";
import {PoolBalanceUpdate} from "@ekubo-v3/types/poolBalanceUpdate.sol";
import {PoolState} from "@ekubo-v3/types/poolState.sol";
import {SwapParameters} from "@ekubo-v3/types/swapParameters.sol";

using FlashAccountantLib for ICore;

// Signed Ekubo V3 (SignedExclusiveSwap) pools set their pool
// config extension to this address; the executor detects a signed hop by
// comparing each hop's poolConfig.extension() against it and routes that hop
// through the signed path.
address constant SIGNED_EXCLUSIVE_SWAP_ADDRESS =
    0x55b703eED01b35641963da2FB2E14885993605A3;

/// Ekubo V3 executor for the Ethereum deployment. Adds the
/// SignedExclusiveSwap extension, whose hops carry a self-describing
/// signature tail, on top of the common extensions in the base executor.
contract EkuboV3EthereumExecutor is EkuboV3ExecutorBase {
    // A signed hop appends meta(32) | minBalanceUpdate(32) | sigLen(2) | sig.
    // These name the fixed-width parts of that tail; the signature length is
    // read from the 2-byte big-endian `sigLen` field.
    uint256 private constant _SIGNED_FIXED_TAIL_LEN = 64;
    uint256 private constant _SIG_LEN_BYTES = 2;

    function _hopEnd(bytes calldata data, uint256 offset, PoolConfig poolConfig)
        internal
        pure
        virtual
        override
        returns (uint256)
    {
        if (poolConfig.extension() != SIGNED_EXCLUSIVE_SWAP_ADDRESS) {
            return offset;
        }

        uint256 sigLenOff = offset + _SIGNED_FIXED_TAIL_LEN;
        if (sigLenOff + _SIG_LEN_BYTES > data.length) {
            revert EkuboV3Executor__InvalidDataLength();
        }
        uint256 sigLen =
            uint256(uint16(bytes2(data[sigLenOff:sigLenOff + _SIG_LEN_BYTES])));
        uint256 sigEnd = sigLenOff + _SIG_LEN_BYTES + sigLen;
        if (sigEnd > data.length) {
            revert EkuboV3Executor__InvalidDataLength();
        }
        return sigEnd;
    }

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
        if (poolKey.config.extension() != SIGNED_EXCLUSIVE_SWAP_ADDRESS) {
            return super._swapHop(poolKey, swapParameters, swapData, offset);
        }

        // Signed hop tail: meta(32) | minBU(32) | sigLen(2) | sig(sigLen).
        // _hopEnd bounds-checks the tail and returns the offset past it.
        nextOffset = _hopEnd(swapData, offset, poolKey.config);
        uint256 sigStart = offset + _SIGNED_FIXED_TAIL_LEN + _SIG_LEN_BYTES;

        // slither-disable-next-line calls-loop
        (balanceUpdate,) = abi.decode(
            CORE.forward(
                SIGNED_EXCLUSIVE_SWAP_ADDRESS,
                abi.encode(
                    poolKey,
                    swapParameters,
                    // SignedSwapMeta (uint256)
                    uint256(bytes32(swapData[offset:offset + 32])),
                    // minBalanceUpdate
                    PoolBalanceUpdate.wrap(
                        bytes32(swapData[offset + 32:offset + 64])
                    ),
                    // signature
                    bytes(swapData[sigStart:nextOffset])
                )
            ),
            (PoolBalanceUpdate, PoolState)
        );
    }
}
