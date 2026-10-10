pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {UniswapV4Executor} from "../src/executors/UniswapV4Executor.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {PoolKey} from "@uniswap/v4-core/src/types/PoolKey.sol";
import {Currency} from "@uniswap/v4-core/src/types/Currency.sol";
import {IHooks} from "@uniswap/v4-core/src/interfaces/IHooks.sol";
import {UniswapV4Utils} from "./protocols/UniswapV4Utils.sol";

/// Loads committed fixtures at the address used by tycho-test, without deploying their source.
contract RuntimeBytecodeFixturesExecutionTest is Test {
    // Keep aligned with EXECUTOR_ADDRESS in crates/tycho-test/src/execution/encoding.rs.
    address constant EXECUTOR_ADDRESS =
        0xaE04CA7E9Ed79cBD988f6c536CE11C621166f41B;

    function testUniswapV4FixturesExecuteCallbacks() public {
        _assertSelfCallReachesPoolManager("UniswapV4", false);
        _assertSelfCallReachesPoolManager("UniswapV4Angstrom", false);
        _assertSelfCallReachesPoolManager("UniswapV4Robinhood", false);
    }

    function testUniswapV4FixturesExecuteWithoutUnlock() public {
        _assertSelfCallReachesPoolManager("UniswapV4", true);
        _assertSelfCallReachesPoolManager("UniswapV4Angstrom", true);
        _assertSelfCallReachesPoolManager("UniswapV4Robinhood", true);
    }

    function _assertSelfCallReachesPoolManager(
        string memory fixtureName,
        bool skipUnlock
    ) internal {
        vm.etch(
            EXECUTOR_ADDRESS,
            vm.parseJsonBytes(
                vm.readFile(
                    string.concat(
                        "../../../protocols/testing/fixtures/",
                        fixtureName,
                        ".runtime.json"
                    )
                ),
                ".runtimeBytecode"
            )
        );
        IPoolManager poolManager =
            UniswapV4Executor(EXECUTOR_ADDRESS).poolManager();
        vm.etch(address(poolManager), hex"00");

        // A marker at the external boundary proves the self-delegatecall executed swap logic.
        // Calling an empty immutable target instead returns success without reaching settle.
        bytes memory marker = abi.encodeWithSignature("ReachedPoolManager()");
        vm.mockCallRevert(
            address(poolManager),
            abi.encodeWithSelector(IPoolManager.settle.selector),
            marker
        );

        address tokenIn = address(0x1000);
        address tokenOut = address(0x2000);
        bytes memory callData;
        if (skipUnlock) {
            UniswapV4Executor.UniswapV4Pool[] memory pools =
                new UniswapV4Executor.UniswapV4Pool[](1);
            pools[0] = UniswapV4Executor.UniswapV4Pool(
                tokenOut, 3000, 60, address(0), ""
            );
            callData = abi.encodeCall(
                UniswapV4Executor.swap,
                (
                    1,
                    UniswapV4Utils.encodeExactInput(
                        tokenIn, tokenOut, true, true, pools
                    ),
                    address(this)
                )
            );
        } else {
            bytes memory swapData = abi.encodeCall(
                UniswapV4Executor.swapExactInputSingle,
                (
                    PoolKey(
                        Currency.wrap(tokenIn),
                        Currency.wrap(tokenOut),
                        3000,
                        60,
                        IHooks(address(0))
                    ),
                    true,
                    uint128(1),
                    address(this),
                    bytes("")
                )
            );
            callData = abi.encodeCall(
                UniswapV4Executor.handleCallback,
                (bytes.concat(new bytes(68), swapData))
            );
        }

        vm.prank(address(poolManager));
        (bool success, bytes memory result) = EXECUTOR_ADDRESS.call(callData);
        assertFalse(success, fixtureName);
        assertEq(
            result,
            abi.encodeWithSignature("Error(string)", string(marker)),
            fixtureName
        );
    }
}
