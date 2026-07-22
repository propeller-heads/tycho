pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    MetricExecutor,
    MetricExecutor__InvalidCallback,
    MetricExecutor__InvalidDataLength
} from "@src/executors/MetricExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";
import "../TychoRouterTestSetup.sol";

error MetricExecutorTest__UnexpectedTransferType();
error MetricDispatcherHarness__NoExecutor();
error MetricDispatcherHarness__UnexpectedTransferType();

interface IMetricSwapCallback {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

contract MetricToken is ERC20 {
    constructor(string memory name, string memory symbol) ERC20(name, symbol) {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

contract MockMetricPool {
    address public immutable token0;
    address public immutable token1;
    uint128 public lastPriceLimitX64;

    constructor(address token0_, address token1_) {
        token0 = token0_;
        token1 = token1_;
    }

    function swap(
        address recipient,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimitX64,
        bytes calldata callbackData,
        bytes calldata extensionData
    ) external returns (int128 amount0Delta, int128 amount1Delta) {
        extensionData;
        lastPriceLimitX64 = priceLimitX64;
        uint256 amountIn = uint256(uint128(amountSpecified));
        uint256 amountOut = amountIn * 2;

        if (zeroForOne) {
            IMetricSwapCallback(msg.sender)
                .metricOmmSwapCallback(
                    int256(amountIn), -int256(amountOut), callbackData
                );
            IERC20(token1).transfer(recipient, amountOut);
            amount0Delta = int128(uint128(amountIn));
            amount1Delta = -int128(uint128(amountOut));
        } else {
            IMetricSwapCallback(msg.sender)
                .metricOmmSwapCallback(
                    -int256(amountOut), int256(amountIn), callbackData
                );
            IERC20(token0).transfer(recipient, amountOut);
            amount0Delta = -int128(uint128(amountOut));
            amount1Delta = int128(uint128(amountIn));
        }
    }
}

contract MetricExecutorExposed is MetricExecutor {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        _payCallback(amount0Delta, amount1Delta, data);
    }

    function _payCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) internal {
        address tokenIn = amount0Delta > 0
            ? MockMetricPool(msg.sender).token0()
            : MockMetricPool(msg.sender).token1();
        uint256 amountOwed =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType != TransferManager.TransferType.Transfer) {
            revert MetricExecutorTest__UnexpectedTransferType();
        }

        IERC20(tokenIn).transfer(receiver, amountOwed);
        handleCallback(msg.data);
        data;
    }
}

contract MetricDispatcherHarness {
    address public currentExecutor;

    function execute(
        address executor,
        uint256 amountIn,
        bytes calldata data,
        address receiver
    ) external {
        currentExecutor = executor;
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(
                IExecutor.swap.selector, amountIn, data, receiver
            )
        );
        currentExecutor = address(0);
        require(success, string(result));
    }

    fallback(bytes calldata data) external returns (bytes memory) {
        address executor = currentExecutor;
        if (executor == address(0)) {
            revert MetricDispatcherHarness__NoExecutor();
        }

        (bool transferDataSuccess, bytes memory transferData) = executor.staticcall(
            abi.encodeWithSelector(
                ICallback.getCallbackTransferData.selector,
                data,
                address(0),
                msg.sender
            )
        );
        require(transferDataSuccess, string(transferData));

        (TransferManager.TransferType transferType, address receiver) =
            abi.decode(transferData, (TransferManager.TransferType, address));
        if (transferType != TransferManager.TransferType.Transfer) {
            revert MetricDispatcherHarness__UnexpectedTransferType();
        }

        (int256 amount0Delta, int256 amount1Delta,) =
            abi.decode(data[4:], (int256, int256, bytes));
        address tokenIn = amount0Delta > 0
            ? MockMetricPool(msg.sender).token0()
            : MockMetricPool(msg.sender).token1();
        uint256 amountOwed =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);

        IERC20(tokenIn).transfer(receiver, amountOwed);

        (bool callbackSuccess, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(ICallback.handleCallback.selector, data)
        );
        require(callbackSuccess, string(result));
        return result;
    }
}

contract MetricExecutorTest is Test {
    MetricToken token0;
    MetricToken token1;
    MetricExecutorExposed executor;

    address receiver = makeAddr("receiver");

    function setUp() public {
        token0 = new MetricToken("Token 0", "TK0");
        token1 = new MetricToken("Token 1", "TK1");
        executor = new MetricExecutorExposed();
    }

    function testGetTransferData() public {
        MockMetricPool pool = _pool();
        bytes memory data = _encodeData(address(pool), true);

        (
            TransferManager.TransferType transferType,
            address transferReceiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(transferReceiver, address(0));
        assertEq(tokenIn, address(token0));
        assertEq(tokenOut, address(token1));
        assertFalse(outputToRouter);
    }

    function testSwap() public {
        MockMetricPool pool = _pool();
        uint256 amountIn = 100 ether;
        token0.mint(address(executor), amountIn);
        token1.mint(address(pool), amountIn * 2);

        executor.swap(amountIn, _encodeData(address(pool), true), receiver);

        assertEq(token0.balanceOf(address(pool)), amountIn);
        assertEq(token1.balanceOf(receiver), amountIn * 2);
        assertEq(token0.balanceOf(address(executor)), 0);
        assertEq(pool.lastPriceLimitX64(), 0);
    }

    function testSwapOneForZeroUsesMaxPriceLimit() public {
        MockMetricPool pool = _pool();
        uint256 amountIn = 100 ether;
        token1.mint(address(executor), amountIn);
        token0.mint(address(pool), amountIn * 2);

        executor.swap(amountIn, _encodeData(address(pool), false), receiver);

        assertEq(token1.balanceOf(address(pool)), amountIn);
        assertEq(token0.balanceOf(receiver), amountIn * 2);
        assertEq(token1.balanceOf(address(executor)), 0);
        assertEq(pool.lastPriceLimitX64(), type(uint128).max);
    }

    function testSwapThroughDispatcherHarness() public {
        MetricDispatcherHarness harness = new MetricDispatcherHarness();
        MetricExecutor implementation = new MetricExecutor();
        MockMetricPool pool = _pool();
        uint256 amountIn = 25 ether;
        token0.mint(address(harness), amountIn);
        token1.mint(address(pool), amountIn * 2);

        harness.execute(
            address(implementation),
            amountIn,
            _encodeData(address(pool), true),
            receiver
        );

        assertEq(token0.balanceOf(address(pool)), amountIn);
        assertEq(token1.balanceOf(receiver), amountIn * 2);
        assertEq(token0.balanceOf(address(harness)), 0);
    }

    function testRejectsTrailingSwapData() public {
        MockMetricPool pool = _pool();
        bytes memory invalid =
            abi.encodePacked(_encodeData(address(pool), true), hex"00");

        vm.expectRevert(MetricExecutor__InvalidDataLength.selector);
        executor.getTransferData(invalid);
    }

    function testRejectsCallbackOutsideSwap() public {
        vm.expectRevert(MetricExecutor__InvalidCallback.selector);
        executor.handleCallback(
            abi.encodeWithSelector(
                IMetricSwapCallback.metricOmmSwapCallback.selector,
                int256(1),
                int256(0),
                ""
            )
        );
    }

    function testRejectsUnknownCallbackSelector() public {
        vm.expectRevert(MetricExecutor__InvalidCallback.selector);
        executor.handleCallback(
            abi.encodeWithSelector(bytes4(0xdeadbeef), int256(1), int256(0), "")
        );
    }

    function _pool() internal returns (MockMetricPool) {
        return new MockMetricPool(address(token0), address(token1));
    }

    function _encodeData(address pool, bool zeroForOne)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            address(token0),
            address(token1),
            pool,
            bytes1(uint8(zeroForOne ? 1 : 0))
        );
    }
}

contract TychoRouterForMetricTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        // Block with a successful interaction on the WETH/USDC MetricOmm pool
        // 0x600668, so its heartbeat oracle is fresh at this height and the
        // swap does not revert FeedStalled. The pool must also hold inventory
        // here (drained pools consume input and return zero output).
        return 48957697;
    }

    function testSingleMetricIntegration() public {
        deal(BASE_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_metric");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(BASE_USDC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
        vm.stopPrank();
    }
}
