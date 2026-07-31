pragma solidity ^0.8.26;

import "@src/executors/UniswapV4Executor.sol";

library UniswapV4Utils {
    function encodeExactInput(
        address tokenIn,
        address tokenOut,
        bool zeroForOne,
        bool skipUnlock,
        UniswapV4Executor.UniswapV4Pool[] memory pools
    ) public pure returns (bytes memory) {
        require(pools.length > 0, "Must have at least one pool");

        bytes memory firstPool = abi.encodePacked(
            pools[0].intermediaryToken,
            bytes3(pools[0].fee),
            pools[0].tickSpacing,
            pools[0].hook,
            bytes2(uint16(pools[0].hookData.length)),
            pools[0].hookData
        );

        bytes[] memory encodedExtraPools = new bytes[](pools.length - 1);
        for (uint256 i = 1; i < pools.length; i++) {
            encodedExtraPools[i - 1] = abi.encodePacked(
                pools[i].intermediaryToken,
                bytes3(pools[i].fee),
                pools[i].tickSpacing,
                pools[i].hook,
                bytes2(uint16(pools[i].hookData.length)),
                pools[i].hookData
            );
        }

        return abi.encodePacked(
            tokenIn,
            tokenOut,
            zeroForOne,
            skipUnlock,
            firstPool,
            pleEncode(encodedExtraPools)
        );
    }

    function pleEncode(bytes[] memory data)
        public
        pure
        returns (bytes memory encoded)
    {
        for (uint256 i = 0; i < data.length; i++) {
            encoded = bytes.concat(
                encoded,
                abi.encodePacked(bytes2(uint16(data[i].length)), data[i])
            );
        }
    }
}
