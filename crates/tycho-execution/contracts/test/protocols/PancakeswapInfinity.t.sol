pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/PancakeswapInfinityExecutor.sol";
import {Constants} from "../Constants.sol";

address constant INFINITY_VAULT = 0x238a358808379702088667322f80aC48bAd5e6c4;
address constant INFINITY_CL_POOL_MANAGER =
    0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b;
address constant INFINITY_BIN_POOL_MANAGER =
    0xC697d2898e0D09264376196696c51D7aBbbAA4a9;
address constant BASE_USDT = 0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2;
// The USDC/USDT pool has liquidity at this block.
uint256 constant INFINITY_FORK_BLOCK = 51600000;

// The 97 bytes `_decodeData` expects for the live USDC/USDT CL pool: fee 5, tick spacing 2 in
// bits [16,40) of `parameters`, no hooks. Same field order as the Rust encoder.
function usdcUsdtSwapData(address tokenIn, address tokenOut, bool zeroForOne)
    pure
    returns (bytes memory)
{
    return abi.encodePacked(
        tokenIn,
        tokenOut,
        zeroForOne,
        uint8(0), // CL
        uint24(5),
        bytes32(uint256(2) << 16),
        address(0)
    );
}

contract PancakeswapInfinityExecutorExposed is PancakeswapInfinityExecutor {
    constructor()
        PancakeswapInfinityExecutor(
            IPancakeswapInfinityVault(INFINITY_VAULT),
            INFINITY_CL_POOL_MANAGER,
            INFINITY_BIN_POOL_MANAGER
        )
    {}

    using SafeERC20 for IERC20;

    /// @dev Stands in for Dispatcher._callHandleCallbackOnExecutor, which moves the input to the
    /// Vault mid-callback. This contract took the lock, so msg.sender is the Vault and
    /// `_handleCallback` is reachable internally. Payload is fixed width: in `lockAcquired(bytes)`
    /// calldata it starts at 68, amountIn first, then tokenIn at payload[52].
    fallback(bytes calldata) external returns (bytes memory) {
        uint256 amountIn = uint256(bytes32(msg.data[68:100]));
        address tokenIn = address(bytes20(msg.data[120:140]));

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType == TransferManager.TransferType.Transfer) {
            IERC20(tokenIn).safeTransfer(receiver, amountIn);
        }
        return _handleCallback(msg.data);
    }

    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 parameters,
            address hooks
        )
    {
        return _decodeData(data);
    }
}

contract PancakeswapInfinityExecutorTest is Constants, TestUtils {
    PancakeswapInfinityExecutorExposed executor;

    function setUp() public {
        executor = new PancakeswapInfinityExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes32 parameters = bytes32(uint256(60) << 16);
        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_USDC,
            true,
            uint8(0),
            uint24(500),
            parameters,
            address(0)
        );
        (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 decodedParameters,
            address hooks
        ) = executor.decodeParams(data);
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertTrue(zeroForOne, "direction byte 1 decodes as zeroForOne");
        assertEq(poolType, 0);
        assertEq(fee, 500);
        assertEq(decodedParameters, parameters);
        assertEq(hooks, address(0));
    }

    function testDecodeParamsRejectsUnknownPoolType() public {
        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_USDC,
            true,
            uint8(2),
            uint24(500),
            bytes32(0),
            address(0)
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                PancakeswapInfinityExecutor__UnknownPoolType.selector, uint8(2)
            )
        );
        executor.decodeParams(data);
    }

    function testDecodeParamsRejectsWrongLength() public {
        vm.expectRevert(PancakeswapInfinityExecutor__InvalidDataLength.selector);
        executor.decodeParams(hex"deadbeef");
    }

    function testCallbackRejectsNonVault() public {
        vm.expectRevert(PancakeswapInfinityExecutor__NotVault.selector);
        executor.handleCallback(new bytes(68));
    }

    /// The payload is read at fixed offsets, so a callback that is not lockAcquired has to be
    /// rejected rather than misparsed into a pool key.
    function testCallbackRejectsUnknownSelector() public {
        vm.prank(INFINITY_VAULT);
        vm.expectRevert(
            abi.encodeWithSelector(
                PancakeswapInfinityExecutor__UnexpectedCallback.selector,
                bytes4(0)
            )
        );
        executor.handleCallback(new bytes(68));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(
            usdcUsdtSwapData(BASE_USDC, BASE_USDT, true)
        );
        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, BASE_USDT);
        assertFalse(outputToRouter, "the Vault pays the receiver directly");
    }

    function testGetCallbackTransferDataErc20() public view {
        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData("", BASE_USDC, address(0));
        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, INFINITY_VAULT);
    }

    function testGetCallbackTransferDataNative() public view {
        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData("", ETH_ADDRESS, address(0));
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
    }

    function testFundsExpectedAddressIsTheCaller() public view {
        assertEq(executor.fundsExpectedAddress(""), address(this));
    }
}

/**
 * Router-level setup check: the executor deploys and is whitelisted with the others in
 * TychoRouterTestSetup.deployExecutors, at the deterministic address committed in
 * config/test_executor_addresses.json ("base" -> "pancakeswap_infinity_cl").
 */
contract TychoRouterForPancakeswapInfinityTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return INFINITY_FORK_BLOCK;
    }

    function testExecutorDeployedAndWhitelisted() public view {
        address executor = address(pancakeswapInfinityExecutor);

        assertTrue(executor != address(0), "executor is not deployed");
        assertTrue(
            tychoRouter.executorsActivationTimestamp(executor) != 0,
            "executor is not whitelisted on the router"
        );
        // Pinned: deployExecutors moves every CREATE address after the executor it
        // inserts, so a reordering has to fail here rather than silently stale the
        // address the Rust encoder tests read.
        assertEq(
            executor,
            0xe54a55121A47451c5727ADBAF9b9FC1643477e25,
            "executor address drifted from test_executor_addresses.json"
        );
    }

    /// The only test that runs the Dispatcher. PancakeswapInfinityForkTest calls the executor
    /// directly through a stand-in, so nothing else covers the balance measurement at the
    /// receiver.
    function testSingleSwap() public {
        uint256 amountIn = 1000e6;
        uint256 expAmountOut = 1000121097;
        deal(BASE_USDC, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDT).balanceOf(ALICE);

        bytes memory swap = encodeSingleSwap(
            address(pancakeswapInfinityExecutor),
            usdcUsdtSwapData(BASE_USDC, BASE_USDT, true)
        );

        vm.startPrank(ALICE);
        IERC20(BASE_USDC).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            BASE_USDC,
            BASE_USDT,
            expAmountOut,
            expAmountOut,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();

        assertEq(amountOut, expAmountOut);
        assertEq(
            IERC20(BASE_USDT).balanceOf(ALICE) - balanceBefore, expAmountOut
        );
        // outputToRouter is false, so the router must never hold either token.
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDT).balanceOf(tychoRouterAddr), 0);
    }
}

contract PancakeswapInfinityForkTest is Constants, TestUtils {
    PancakeswapInfinityExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), INFINITY_FORK_BLOCK);
        executor = new PancakeswapInfinityExecutorExposed();
    }

    /// @dev Funds the executor with tokenIn, swaps all of it and checks it reached ALICE as
    /// tokenOut.
    function _assertSwap(address tokenIn, address tokenOut, bytes memory data)
        internal
    {
        uint256 amountIn = 1000e6;
        deal(tokenIn, address(executor), amountIn);
        executor.swap(amountIn, data, ALICE);
        assertGt(IERC20(tokenOut).balanceOf(ALICE), 0);
        assertEq(IERC20(tokenIn).balanceOf(address(executor)), 0);
    }

    function testSwapUsdcToUsdt() public {
        _assertSwap(
            BASE_USDC, BASE_USDT, usdcUsdtSwapData(BASE_USDC, BASE_USDT, true)
        );
    }

    function testSwapUsdtToUsdc() public {
        _assertSwap(
            BASE_USDT, BASE_USDC, usdcUsdtSwapData(BASE_USDT, BASE_USDC, false)
        );
    }

    function testSwapMatchesEncoderCalldata() public {
        bytes memory data =
            loadCallDataFromFile("test_encode_pancakeswap_infinity_cl_swap");
        assertEq(data.length, 97);
        _assertSwap(BASE_USDC, BASE_USDT, data);
    }
}
