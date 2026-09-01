// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

library LibSwap {
    /**
     * @dev Sentinel value in a hop's executor slot marking that the hop
     * carries a fallback bundle instead of a plain executor + protocol data
     * pair. `address(0)` is never an approved executor, so the marker is
     * unambiguous. Hops without a fallback keep the existing layout.
     */
    address internal constant FALLBACK_MARKER = address(0);

    /**
     * @dev Returns arguments required to perform a single swap
     */
    function decodeSingleSwap(bytes calldata swap)
        internal
        pure
        returns (address executor, bytes calldata protocolData)
    {
        executor = address(uint160(bytes20(swap[0:20])));
        protocolData = swap[20:];
    }

    /**
     * @dev Returns arguments required to perform a sequential swap
     */
    function decodeSequentialSwap(bytes calldata swap)
        internal
        pure
        returns (address executor, bytes calldata protocolData)
    {
        executor = address(uint160(bytes20(swap[0:20])));
        protocolData = swap[20:];
    }

    /**
     * @dev Returns arguments required to perform a split swap
     */
    function decodeSplitSwap(bytes calldata swap)
        internal
        pure
        returns (uint8 tokenInIndex, uint8 tokenOutIndex, uint24 split, address executor, bytes calldata protocolData)
    {
        tokenInIndex = uint8(swap[0]);
        tokenOutIndex = uint8(swap[1]);
        split = uint24(bytes3(swap[2:5]));
        executor = address(uint160(bytes20(swap[5:25])));
        protocolData = swap[25:];
    }

    /**
     * @dev Decodes a fallback bundle. `data` is the payload that follows the
     * `FALLBACK_MARKER` executor slot:
     * `uint16 primaryLength || primary || fallback`, where primary and
     * fallback are each `executor (20 bytes) || protocolData` and
     * primaryLength is the byte length of the primary pair.
     */
    function decodeFallbackSwap(bytes calldata data)
        internal
        pure
        returns (
            address executor,
            bytes calldata protocolData,
            address fallbackExecutor,
            bytes calldata fallbackData
        )
    {
        uint256 primaryLength = uint16(bytes2(data[0:2]));
        executor = address(uint160(bytes20(data[2:22])));
        protocolData = data[22:2 + primaryLength];
        fallbackExecutor =
            address(uint160(bytes20(data[2 + primaryLength:22 + primaryLength])));
        fallbackData = data[22 + primaryLength:];
    }
}
